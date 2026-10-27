use std::ffi::CStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use rapira_scoreboard::SharedSlot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::scoreboard::{Event, count_shed, sb_update};
use crate::{zend_class_entry, zend_object};

pub(crate) const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// The body bytes that one worker holds for the units that PHP has not taken yet.
const BODY_BUDGET: usize = 64 << 20;

/// The cycle bookkeeping view of a unit that receive() handed out.
pub trait Held {
    /// The worker committed the outcome, or discarded the unit.
    fn settled(&self) -> bool;
    /// The unit takes no outcome: the client left, or the plugin closed the unit. Work::isCancelled().
    fn cancelled(&self) -> bool;
    fn discard(&mut self);

    /// Work::isFinalized().
    fn is_finalized(&self) -> bool {
        self.settled() || self.cancelled()
    }
}

/// Reclaims the Box that receive() handed out: clears the cycle slot if it still points here, and counts an unfinalized unit as handled.
/// # Safety
/// `ptr` came from `Box::into_raw` in receive and was not reclaimed before.
pub unsafe fn release<T: Held + ?Sized>(ptr: *mut T) -> Box<T> {
    crate::exchange::forget_held(ptr.cast::<()>());
    let st = unsafe { Box::from_raw(ptr) };
    if !st.settled() {
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
    /// The classic and worker modes.
    fn into_cgi(self: Box<Self>) -> crate::types::Context;
    /// A worker that cannot serve: the plugin's refusal, 503 or UNAVAILABLE.
    fn shed(self: Box<Self>);
}

/// One entry of the intake.
pub(crate) enum Entry {
    /// A unit and its body bytes in the budget. The pull drops the permit, and the bytes go back.
    Unit {
        unit: Box<dyn Work>,
        _bytes: OwnedSemaphorePermit,
    },
    /// A reload: the app instance takes no more units, and the next one takes the rest.
    RestartApp,
}

/// The class entries of one plugin's dispatcher surface. Set once per worker.
#[derive(Clone, Copy)]
pub struct DispatcherClasses {
    pub dispatcher: unsafe fn() -> *mut zend_class_entry,
    pub info: unsafe fn() -> *mut zend_class_entry,
    pub unit: unsafe fn() -> *mut zend_class_entry,
    /// The error of receive() and tryReceive() while a unit is unfinalized.
    pub busy: &'static CStr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Saturated,
    OverBudget,
    Stopped,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saturated => write!(f, "worker pool saturated for {INTAKE_WAIT:?}"),
            Self::OverBudget => write!(
                f,
                "request bodies that wait for the worker use all {} MiB",
                BODY_BUDGET >> 20
            ),
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
pub struct Intake {
    tx: SyncSender<Entry>,
    /// The scoreboard slot of this worker.
    slot: &'static SharedSlot,
    /// The body bytes of the units between submit and the PHP thread. All clones share it.
    budget: Arc<Semaphore>,
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

impl Intake {
    pub(crate) fn new(tx: SyncSender<Entry>, slot: &'static SharedSlot) -> Self {
        Self {
            tx,
            slot,
            budget: Arc::new(Semaphore::new(BODY_BUDGET)),
        }
    }

    /// `bytes` is the body size of the unit. A multipart body that the plugin parsed keeps its file parts in spool files and its other parts in memory. submit refuses a body that does not fit the free budget immediately, because the body stays in memory or in the spool dir while it waits. A spool dir on tmpfs is also memory.
    /// pending is incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero.
    pub async fn submit(&self, unit: Box<dyn Work>, bytes: usize) -> Result<(), Refused> {
        // A body larger than the budget takes all of it.
        let Ok(held) =
            Arc::clone(&self.budget).try_acquire_many_owned(bytes.min(BODY_BUDGET) as u32)
        else {
            count_shed(self.slot);
            return Err(Refused::OverBudget);
        };
        let mut entry = Entry::Unit { unit, _bytes: held };
        let mut pending = PendingGuard::arm(self.slot);
        loop {
            match self.tx.try_send(entry) {
                Ok(()) => {
                    pending.disarm();
                    return Ok(());
                }
                Err(TrySendError::Full(e)) => {
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
                    entry = e;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(Refused::Stopped),
            }
        }
    }

    /// Sends the reload signal through the intake, so it also wakes a PHP thread that waits for work.
    /// It is no unit: it takes no budget, no counter sees it, and it waits for room with no deadline.
    pub(crate) async fn restart_app(&self) -> Result<(), Refused> {
        let mut entry = Entry::RestartApp;
        loop {
            match self.tx.try_send(entry) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(e)) => {
                    entry = e;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(Refused::Stopped),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;

    struct Probe;
    impl Work for Probe {
        fn cancelled(&self) -> bool {
            false
        }
        unsafe fn attach(self: Box<Self>, _: *mut zend_object) -> *mut dyn Held {
            unreachable!()
        }
        fn into_cgi(self: Box<Self>) -> crate::types::Context {
            unreachable!()
        }
        fn shed(self: Box<Self>) {}
    }

    /// An intake on slot 0 of a board in memory.
    fn intake(tx: SyncSender<Entry>) -> Intake {
        Intake::new(tx, &rapira_scoreboard::create(1).unwrap()[0])
    }

    #[tokio::test(flavor = "current_thread")]
    async fn intake_reports_stopped_after_the_receiver_is_gone() {
        let (tx, rx) = sync_channel(1);
        let intake = intake(tx);
        let slot = intake.slot;
        drop(rx);
        assert_eq!(
            intake.submit(Box::new(Probe), 0).await.unwrap_err(),
            Refused::Stopped
        );
        assert_eq!(
            (
                slot.failed_on_full_queue.load(Ordering::Relaxed),
                slot.pending.load(Ordering::Relaxed),
            ),
            (0, 0),
            "the unit never found the queue full: failed_on_full_queue, pending"
        );
    }

    /// A reload waits for room in a full intake with no deadline. It is a signal, not a unit: after it enters, only the unit that filled the queue counts as pending, and nothing counts as failed or shed.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_reload_waits_for_room_without_a_deadline() {
        let (tx, rx) = sync_channel(1);
        let intake = intake(tx);
        let slot = intake.slot;
        intake.submit(Box::new(Probe), 0).await.unwrap();
        let (sent, _) = tokio::join!(intake.restart_app(), async {
            tokio::time::sleep(INTAKE_WAIT * 2).await;
            rx.try_recv().unwrap()
        });
        assert_eq!(sent, Ok(()), "the reload enters once there is room");
        assert_eq!(
            (
                slot.handled.load(Ordering::Relaxed),
                slot.errors.load(Ordering::Relaxed),
                slot.failed_on_full_queue.load(Ordering::Relaxed),
                slot.pending.load(Ordering::Relaxed),
            ),
            (0, 0, 0, 1),
            "handled, errors, failed_on_full_queue, pending"
        );
    }

    /// Each case fills a queue of one with a reload entry before the intake takes the sender, so `pending` does not count it and the submit finds the queue full. The paused clock moves to the next timer at once, so the waits take no real time.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_full_intake_counts_each_unit_that_never_enters() {
        enum During {
            Nothing,
            ClientLeaves,
            WorkerStops,
            RoomFrees,
        }
        struct Counts {
            handled: u64,
            errors: u64,
            failed_on_full_queue: u64,
            pending: u64,
        }
        struct Case {
            name: &'static str,
            /// What happens 1 s into the wait.
            during: During,
            /// None: the client left, so the submit gives no result.
            result: Option<Result<(), Refused>>,
            want: Counts,
        }
        const ACTION_DELAY: Duration = Duration::from_secs(1);
        let cases = [
            Case {
                name: "shed after the wait",
                during: During::Nothing,
                result: Some(Err(Refused::Saturated)),
                want: Counts {
                    handled: 1,
                    errors: 1,
                    failed_on_full_queue: 1,
                    pending: 0,
                },
            },
            Case {
                name: "the client leaves during the wait",
                during: During::ClientLeaves,
                result: None,
                want: Counts {
                    handled: 0,
                    errors: 0,
                    failed_on_full_queue: 1,
                    pending: 0,
                },
            },
            Case {
                name: "the worker stops during the wait",
                during: During::WorkerStops,
                result: Some(Err(Refused::Stopped)),
                want: Counts {
                    handled: 0,
                    errors: 0,
                    failed_on_full_queue: 1,
                    pending: 0,
                },
            },
            Case {
                name: "room frees during the wait",
                during: During::RoomFrees,
                result: Some(Ok(())),
                // The unit is in the queue. Only the pull of the PHP thread subtracts it from `pending`.
                want: Counts {
                    handled: 0,
                    errors: 0,
                    failed_on_full_queue: 0,
                    pending: 1,
                },
            },
        ];
        for case in cases {
            let (tx, rx) = sync_channel::<Entry>(1);
            tx.send(Entry::RestartApp).unwrap();
            let intake = intake(tx);
            let slot = intake.slot;
            let submit = intake.submit(Box::new(Probe), 0);
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
                (
                    case.want.handled,
                    case.want.errors,
                    case.want.failed_on_full_queue,
                    case.want.pending,
                ),
                "{}: handled, errors, failed_on_full_queue, pending",
                case.name
            );
        }
    }
}
