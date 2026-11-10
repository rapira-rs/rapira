use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Duration;

use rapira_scoreboard::SharedSlot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::callbacks::guard;
use crate::scoreboard::{Event, count_shed, sb_update};
use crate::types::Context;

pub(crate) const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// The units one worker holds for PHP.
pub(crate) const INTAKE_SLOTS: usize = 1024;

/// The body bytes that one worker holds for the units that PHP has not taken yet.
const BODY_BUDGET: usize = 64 << 20;

/// A dispatcher-mode unit, from submit to free_obj: the plugin state behind the object receive() hands to PHP.
pub trait Held: Send + 'static {
    /// The worker committed the outcome, or discarded the unit.
    fn settled(&self) -> bool;
    /// The unit takes no outcome: the client left, or the plugin closed the unit. Work::isCancelled(). The pull skips a queued unit that is cancelled.
    fn cancelled(&self) -> bool;
    fn discard(&mut self);
    /// The object of an unsettled unit is gone: what the client gets in place of the outcome. The drop shell skips it when the cycle died.
    fn lost(&mut self);
    /// A worker that cannot serve: the plugin's refusal, 503 or UNAVAILABLE.
    fn shed(self: Box<Self>);

    /// Work::isFinalized().
    fn is_finalized(&self) -> bool {
        self.settled() || self.cancelled()
    }
}

/// Reclaims the Box that receive() handed out: clears the cycle slot if it still points here, and counts an unsettled unit as handled.
/// # Safety
/// `ptr` came from `Box::leak` in receive and was not reclaimed before.
pub(crate) unsafe fn release(ptr: *mut dyn Held) -> Box<dyn Held> {
    crate::exchange::forget_held(ptr.cast::<()>());
    let st = unsafe { Box::from_raw(ptr) };
    if !st.settled() {
        sb_update(Event::Handled { errored: true });
    }
    st
}

/// The unit behind a Work object: the first field of the C prefix of every unit class (rapira_held in rapira_sapi.h). zend_object_alloc zeroes the prefix, so a fresh object holds None.
#[repr(transparent)]
pub struct HeldPtr(pub(crate) Option<NonNull<dyn Held>>);

// rapira_held mirrors the two words of the fat pointer, and the zeroed prefix is None.
const _: () = assert!(
    size_of::<HeldPtr>() == 2 * size_of::<*const ()>()
        && align_of::<HeldPtr>() == align_of::<*const ()>()
);
const _: () = assert!(
    unsafe { MaybeUninit::<HeldPtr>::zeroed().assume_init() }
        .0
        .is_none()
);

impl HeldPtr {
    fn get(&self) -> NonNull<dyn Held> {
        self.0.expect("a unit object that receive() never attached")
    }

    /// The plugin state that receive() attached.
    /// # Safety
    /// receive() attached a `T` to this slot, and the object is live.
    pub unsafe fn state<T>(&self) -> *mut T {
        self.get().cast::<T>().as_ptr()
    }
}

/// Work::isFinalized().
/// # Safety
/// `held` is the held word of a live unit object that receive() attached.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_is_finalized(held: *const HeldPtr) -> bool {
    guard(false, || unsafe { (*held).get().as_ref().is_finalized() })
}

/// Work::isCancelled().
/// # Safety
/// As `rapira_rs_held_is_finalized`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_is_cancelled(held: *const HeldPtr) -> bool {
    guard(false, || unsafe { (*held).get().as_ref().cancelled() })
}

/// The free_obj of a unit class: reclaims the unit, and an unsettled unit sends its lost outcome. A cycle that died (a fatal error, a timeout) skips the outcome: the plugin reports the worker death when the sender drops.
/// # Safety
/// `held` is the held word of the object that free_obj got; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_drop(held: *mut HeldPtr) {
    guard((), || unsafe {
        // The receive shells allocate the object before the pull: a busy refusal, a timeout, an empty tryReceive() or a closed source frees an object with no unit.
        let Some(ptr) = (*held).0.take() else {
            return;
        };
        let mut held = release(ptr.as_ptr());
        if !held.settled() && !crate::compiler_globals.unclean_shutdown {
            held.lost();
        }
    })
}

/// One unit on the intake: the CGI reply of the classic and worker modes, or a dispatcher-mode unit.
pub enum Unit {
    Cgi(Box<Context>),
    Dispatcher(Box<dyn Held>),
}

impl Unit {
    /// The client left while the unit was queued: the pull skips it.
    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::Cgi(ctx) => ctx.sender.as_ref().is_none_or(|tx| tx.is_closed()),
            Self::Dispatcher(held) => held.cancelled(),
        }
    }

    /// A worker that cannot serve: 503 for a CGI unit, the plugin's refusal for a dispatcher unit.
    pub(crate) fn shed(self) {
        match self {
            Self::Cgi(mut ctx) => ctx.refuse(503),
            Self::Dispatcher(held) => held.shed(),
        }
    }

    /// The classic and worker modes take the CGI reply.
    pub(crate) fn into_cgi(self) -> Box<Context> {
        match self {
            Self::Cgi(ctx) => ctx,
            Self::Dispatcher(_) => unreachable!("a dispatcher unit on a classic or worker intake"),
        }
    }

    /// Dispatcher mode: the unit behind the object.
    pub(crate) fn into_held(self) -> Box<dyn Held> {
        match self {
            Self::Dispatcher(held) => held,
            Self::Cgi(_) => unreachable!("a CGI unit on a dispatcher intake"),
        }
    }
}

/// One entry of the intake.
pub(crate) enum Entry {
    /// A unit, its body bytes in the budget and its slot. The pull drops both permits: the bytes go back, and the oldest waiting submit gets the slot.
    Unit {
        unit: Unit,
        _bytes: OwnedSemaphorePermit,
        _slot: OwnedSemaphorePermit,
    },
    /// A reload: the app instance takes no more units, and the next one takes the rest.
    RestartApp,
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
    tx: Sender<Entry>,
    /// The scoreboard slot of this worker.
    slot: &'static SharedSlot,
    /// The body bytes of the units between submit and the PHP thread. All clones share it.
    budget: Arc<Semaphore>,
    /// The units between submit and the PHP thread, at most INTAKE_SLOTS. A submit that finds no slot waits without a poll; the semaphore wakes the waiters in arrival order.
    slots: Arc<Semaphore>,
}

/// Keeps one unit in `pending` during the hand-off. A drop before `disarm` subtracts the unit from `pending`. If the unit found the queue full, the drop also adds 1 to `failed_on_full_queue`.
struct PendingGuard {
    slot: Option<&'static SharedSlot>,
    /// The unit found every slot taken.
    found_full: bool,
}

impl PendingGuard {
    fn arm(slot: &'static SharedSlot) -> Self {
        slot.pending.fetch_add(1, Ordering::Relaxed);
        Self {
            slot: Some(slot),
            found_full: false,
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
            if self.found_full {
                slot.failed_on_full_queue.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Intake {
    pub(crate) fn new(tx: Sender<Entry>, slot: &'static SharedSlot) -> Self {
        Self {
            tx,
            slot,
            budget: Arc::new(Semaphore::new(BODY_BUDGET)),
            slots: Arc::new(Semaphore::new(INTAKE_SLOTS)),
        }
    }

    /// `bytes` is the body size of the unit. A multipart body that the plugin parsed keeps its file parts in spool files and its other parts in memory. submit refuses a body that does not fit the free budget immediately, because the body stays in memory or in the spool dir while it waits. A spool dir on tmpfs is also memory.
    /// pending is incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero.
    pub async fn submit(&self, unit: Unit, bytes: usize) -> Result<(), Refused> {
        // A body larger than the budget takes all of it.
        let Ok(held) =
            Arc::clone(&self.budget).try_acquire_many_owned(bytes.min(BODY_BUDGET) as u32)
        else {
            count_shed(self.slot);
            return Err(Refused::OverBudget);
        };
        let mut pending = PendingGuard::arm(self.slot);
        let slot = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(slot) => slot,
            Err(_) => {
                pending.found_full = true;
                let wait = Arc::clone(&self.slots).acquire_owned();
                let Ok(slot) = tokio::time::timeout(INTAKE_WAIT, wait).await else {
                    tracing::warn!(
                        target: "rapira",
                        "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                        self.slot.pending.load(Ordering::Relaxed)
                    );
                    count_shed(self.slot);
                    return Err(Refused::Saturated);
                };
                slot.expect("the intake never closes its slot semaphore")
            }
        };
        let entry = Entry::Unit {
            unit,
            _bytes: held,
            _slot: slot,
        };
        if self.tx.send(entry).is_err() {
            return Err(Refused::Stopped);
        }
        pending.disarm();
        Ok(())
    }

    /// Sends the reload signal through the intake, so it also wakes a PHP thread that waits for work.
    /// It is no unit: it takes no slot and no budget, and no counter sees it, so it enters a full intake at once.
    pub(crate) fn restart_app(&self) -> Result<(), Refused> {
        self.tx
            .send(Entry::RestartApp)
            .map_err(|_| Refused::Stopped)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use super::*;

    struct Probe;
    impl Held for Probe {
        fn settled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> bool {
            false
        }
        fn discard(&mut self) {}
        fn lost(&mut self) {}
        fn shed(self: Box<Self>) {}
    }

    fn probe() -> Unit {
        Unit::Dispatcher(Box::new(Probe))
    }

    /// The shed of a CGI unit is the 503 seal.
    #[test]
    fn a_cgi_shed_refuses_with_503() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut ctx = crate::types::tests::context();
        ctx.sender = Some(tx);
        Unit::Cgi(Box::new(ctx)).shed();
        let mut got = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            got.push(crate::types::tests::describe(frame));
        }
        assert_eq!(
            got,
            ["head 503 cl=Some(0) bodiless=false", "end truncated=false"]
        );
    }

    /// An intake on slot 0 of a board in memory.
    fn intake(tx: Sender<Entry>) -> Intake {
        Intake::new(tx, &rapira_scoreboard::create(1).unwrap()[0])
    }

    #[tokio::test(flavor = "current_thread")]
    async fn intake_reports_stopped_after_the_receiver_is_gone() {
        let (tx, rx) = channel();
        let intake = intake(tx);
        let slot = intake.slot;
        drop(rx);
        assert_eq!(
            intake.submit(probe(), 0).await.unwrap_err(),
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

    /// A reload enters a full intake at once: it is a signal, not a unit, so it never waits for a unit to leave and no counter sees it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_reload_enters_a_full_intake_at_once() {
        let (tx, _rx) = channel();
        let intake = intake(tx);
        let slot = intake.slot;
        for _ in 0..INTAKE_SLOTS {
            intake.submit(probe(), 0).await.unwrap();
        }
        assert_eq!(
            intake.restart_app(),
            Ok(()),
            "the reload enters without a wait"
        );
        assert_eq!(
            (
                slot.handled.load(Ordering::Relaxed),
                slot.errors.load(Ordering::Relaxed),
                slot.failed_on_full_queue.load(Ordering::Relaxed),
                slot.pending.load(Ordering::Relaxed),
            ),
            (0, 0, 0, INTAKE_SLOTS as u64),
            "handled, errors, failed_on_full_queue, pending"
        );
    }

    /// Each case fills all INTAKE_SLOTS slots first, so the submit under test finds the intake full. The paused clock moves to the next timer at once, so the waits take no real time.
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
            /// Beyond the INTAKE_SLOTS units that fill the intake.
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
            let (tx, rx) = channel::<Entry>();
            let intake = intake(tx);
            let slot = intake.slot;
            for _ in 0..INTAKE_SLOTS {
                intake.submit(probe(), 0).await.unwrap();
            }
            let submit = intake.submit(probe(), 0);
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
                    let (result, ()) = tokio::join!(submit, async {
                        tokio::time::sleep(ACTION_DELAY).await;
                        // The pull drops the entry, and its slot goes to the waiting submit.
                        drop(rx.try_recv().unwrap());
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
                    case.want.pending + INTAKE_SLOTS as u64,
                ),
                "{}: handled, errors, failed_on_full_queue, pending",
                case.name
            );
        }
    }
}
