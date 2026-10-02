use std::ffi::CStr;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use rapira_scoreboard::SharedSlot;
use tokio::time::Instant;

use crate::scoreboard::{Event, count_shed, sb_update};
use crate::{zend_class_entry, zend_object};

pub(crate) const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// The cycle bookkeeping view of a unit that receive() handed out.
pub trait Held {
    /// The worker committed the outcome, or discarded the unit.
    fn finalized(&self) -> bool;
    /// The unit takes no outcome: the client left, or the plugin closed the unit. Work::isCancelled().
    fn client_closed(&self) -> bool;
    fn discard(&mut self);

    /// Work::isFinalized().
    fn is_finalized(&self) -> bool {
        self.finalized() || self.client_closed()
    }
}

/// Reclaims the Box that receive() handed out: clears the cycle slot if it still points here, and counts an unfinalized unit as handled.
/// # Safety
/// `ptr` came from `Box::into_raw` in receive and was not reclaimed before.
pub unsafe fn release<T: Held + ?Sized>(ptr: *mut T) -> Box<T> {
    crate::exchange::forget_held(ptr.cast::<()>());
    let st = unsafe { Box::from_raw(ptr) };
    if !st.finalized() {
        sb_update(Event::Handled(true));
    }
    st
}

/// One unit of work on the intake. The PHP thread sees only this trait.
pub trait Work: Send + 'static {
    /// The client left while the unit was queued: receive() skips it.
    fn cancelled(&self) -> bool;
    /// Dispatcher mode: attaches the unit to the object receive() allocated from `DispatcherClasses::unit`.
    /// # Safety
    /// `obj` is a live object of that class on the PHP thread.
    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held;
    /// The classic and worker modes. None: this unit cannot run there.
    fn into_cgi(self: Box<Self>) -> Option<crate::types::Context>;
    /// A worker that cannot serve: the plugin's refusal, 503 or UNAVAILABLE.
    fn shed(self: Box<Self>);
}

/// The class entries of one plugin's dispatcher surface. Set once per worker.
#[derive(Clone, Copy)]
pub struct DispatcherClasses {
    pub dispatcher: unsafe fn() -> *mut zend_class_entry,
    pub info: unsafe fn() -> *mut zend_class_entry,
    pub unit: unsafe fn() -> *mut zend_class_entry,
    /// The receive() error while a unit is unfinalized.
    pub busy: &'static CStr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Saturated,
    Stopped,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saturated => write!(f, "worker pool saturated for {INTAKE_WAIT:?}"),
            Self::Stopped => write!(f, "worker pool stopped"),
        }
    }
}

impl std::error::Error for Refused {}

pub fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The queue from a plugin thread to the PHP thread. Clone per plugin task.
#[derive(Clone)]
pub struct Sink {
    tx: SyncSender<Box<dyn Work>>,
    /// The scoreboard slot of this worker.
    slot: &'static SharedSlot,
}

/// Keeps one unit in `pending` during the hand-off. A drop before `disarm` subtracts the unit from `pending`. If the unit found the queue full, the drop also adds 1 to `failed_on_full_queue`.
struct PendingGuard {
    slot: Option<&'static SharedSlot>,
    /// The time to shed the unit. It is set when the unit first finds the queue full, so the common send needs no clock read.
    deadline: Option<Instant>,
}

impl PendingGuard {
    fn arm(slot: &'static SharedSlot) -> Self {
        slot.pending.fetch_add(1, Ordering::Relaxed);
        Self {
            slot: Some(slot),
            deadline: None,
        }
    }
    fn disarm(mut self) {
        self.slot = None;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.pending.fetch_sub(1, Ordering::Relaxed);
            if self.deadline.is_some() {
                slot.failed_on_full_queue.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Sink {
    pub(crate) fn new(tx: SyncSender<Box<dyn Work>>, slot: &'static SharedSlot) -> Self {
        Self { tx, slot }
    }

    /// pending is incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero.
    pub async fn submit(&self, mut unit: Box<dyn Work>) -> Result<(), Refused> {
        let mut pending = PendingGuard::arm(self.slot);
        loop {
            match self.tx.try_send(unit) {
                Ok(()) => {
                    pending.disarm();
                    return Ok(());
                }
                Err(TrySendError::Full(u)) => {
                    let deadline = *pending
                        .deadline
                        .get_or_insert_with(|| Instant::now() + INTAKE_WAIT);
                    if Instant::now() > deadline {
                        tracing::warn!(
                            target: "rapira",
                            "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                            self.slot.pending.load(Ordering::Relaxed)
                        );
                        count_shed(self.slot);
                        return Err(Refused::Saturated);
                    }
                    unit = u;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(Refused::Stopped),
            }
        }
    }
}

/// The typed handle a plugin's transport submits to.
pub struct Intake<U: Work> {
    sink: Sink,
    unit: PhantomData<fn(U)>,
}

impl<U: Work> Clone for Intake<U> {
    fn clone(&self) -> Self {
        Self::new(self.sink.clone())
    }
}

impl<U: Work> Intake<U> {
    /// The worker behind `sink` must have started with the `DispatcherClasses` of the plugin that owns `U`.
    pub fn new(sink: Sink) -> Self {
        Self {
            sink,
            unit: PhantomData,
        }
    }

    pub async fn submit(&self, unit: U) -> Result<(), Refused> {
        self.sink.submit(Box::new(unit)).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use rapira_scoreboard::Scoreboard;

    use super::*;

    struct Probe;
    impl Work for Probe {
        fn cancelled(&self) -> bool {
            false
        }
        unsafe fn attach(self: Box<Self>, _: *mut zend_object) -> *mut dyn Held {
            unreachable!()
        }
        fn into_cgi(self: Box<Self>) -> Option<crate::types::Context> {
            None
        }
        fn shed(self: Box<Self>) {}
    }

    /// A sink on slot 0 of a board in memory.
    fn sink(tx: SyncSender<Box<dyn Work>>) -> (Sink, &'static SharedSlot) {
        let slot = Scoreboard::create(1).unwrap().slot(0);
        (Sink::new(tx, slot), slot)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn intake_reports_stopped_after_the_receiver_is_gone() {
        let (tx, rx) = sync_channel(1);
        let (sink, slot) = sink(tx);
        let intake = Intake::<Probe>::new(sink);
        drop(rx);
        assert_eq!(intake.submit(Probe).await.unwrap_err(), Refused::Stopped);
        assert_eq!(
            (
                slot.failed_on_full_queue.load(Ordering::Relaxed),
                slot.pending.load(Ordering::Relaxed),
            ),
            (0, 0),
            "the unit never found the queue full: failed_on_full_queue, pending"
        );
    }

    /// Each case fills a queue of one before the sink takes the sender, so `pending` does not count that unit and the submit finds the queue full. The paused clock moves to the next timer at once, so the waits take no real time.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_full_intake_counts_each_unit_that_never_enters() {
        enum During {
            Nothing,
            ClientLeaves,
            WorkerStops,
            RoomFrees,
        }
        struct Case {
            name: &'static str,
            /// What happens 1 s into the wait.
            during: During,
            /// None: the client left, so the submit gives no result.
            result: Option<Result<(), Refused>>,
            /// handled, errors, failed_on_full_queue, pending.
            want: (u64, u64, u64, u64),
        }
        const ACTION_DELAY: Duration = Duration::from_secs(1);
        let cases = [
            Case {
                name: "shed after the wait",
                during: During::Nothing,
                result: Some(Err(Refused::Saturated)),
                want: (1, 1, 1, 0),
            },
            Case {
                name: "the client leaves during the wait",
                during: During::ClientLeaves,
                result: None,
                want: (0, 0, 1, 0),
            },
            Case {
                name: "the worker stops during the wait",
                during: During::WorkerStops,
                result: Some(Err(Refused::Stopped)),
                want: (0, 0, 1, 0),
            },
            Case {
                name: "room frees during the wait",
                during: During::RoomFrees,
                result: Some(Ok(())),
                // The unit is in the queue. Only the pull of the PHP thread subtracts it from `pending`.
                want: (0, 0, 0, 1),
            },
        ];
        for case in cases {
            let (tx, rx) = sync_channel::<Box<dyn Work>>(1);
            tx.send(Box::new(Probe)).unwrap();
            let (sink, slot) = sink(tx);
            let intake = Intake::<Probe>::new(sink);
            let submit = intake.submit(Probe);
            let result = match case.during {
                During::Nothing => Some(submit.await),
                During::ClientLeaves => tokio::time::timeout(ACTION_DELAY, submit).await.ok(),
                During::WorkerStops => {
                    let (result, ()) = tokio::join!(submit, async move {
                        tokio::time::sleep(ACTION_DELAY).await;
                        drop(rx);
                    });
                    Some(result)
                }
                During::RoomFrees => {
                    let (result, _) = tokio::join!(submit, async {
                        tokio::time::sleep(ACTION_DELAY).await;
                        rx.try_recv().unwrap()
                    });
                    Some(result)
                }
            };
            assert_eq!(result, case.result, "{}", case.name);
            assert_eq!(
                (
                    slot.handled.load(Ordering::Relaxed),
                    slot.errors.load(Ordering::Relaxed),
                    slot.failed_on_full_queue.load(Ordering::Relaxed),
                    slot.pending.load(Ordering::Relaxed),
                ),
                case.want,
                "{}: handled, errors, failed_on_full_queue, pending",
                case.name
            );
        }
    }
}
