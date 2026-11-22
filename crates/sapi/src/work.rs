use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use rapira_scoreboard::Pool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::callbacks::guard;
use crate::scoreboard::{Event, sb_update};
use crate::types::Context;
use crate::zend_class_entry;

pub(crate) const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// The number of slots in one pool queue.
pub(crate) const INTAKE_SLOTS: usize = 1024;

/// The body bytes that one pool holds for pending units.
const BODY_BUDGET: usize = 64 << 20;

/// A dispatcher-mode unit from submit to free_obj. It is the plugin state behind the object that receive() hands to PHP.
pub trait Held: Send + 'static {
    /// The worker committed the outcome, or discarded the unit.
    fn settled(&self) -> bool;
    /// The unit takes no outcome, because the client left or the plugin closed the unit. Work::isCancelled() returns this value. The pull skips a queued unit that is cancelled.
    fn cancelled(&self) -> bool;
    fn discard(&mut self);
    /// Sends what the client gets in place of the outcome when the object of an unsettled unit is gone. The drop shell skips it when the cycle died.
    fn lost(&mut self);
    /// Sheds the unit with the refusal of the plugin, 503 or UNAVAILABLE, when the worker cannot serve it.
    fn shed(self: Box<Self>);
    /// The class of the object that receive() creates for this unit. The plugin registered it in MINIT, so it is never null.
    fn class(&self) -> *mut zend_class_entry;

    /// Work::isFinalized() returns this value.
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

/// The unit behind a Work object. It is the first field of the C prefix of every unit class (rapira_held in rapira_sapi.h). zend_object_alloc zeroes the prefix, so a fresh object holds None.
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

/// Work::isFinalized() returns this value.
/// # Safety
/// `held` is the held word of a live unit object that receive() attached.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_is_finalized(held: *const HeldPtr) -> bool {
    guard(false, || unsafe { (*held).get().as_ref().is_finalized() })
}

/// Work::isCancelled() returns this value.
/// # Safety
/// As `rapira_rs_held_is_finalized`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_is_cancelled(held: *const HeldPtr) -> bool {
    guard(false, || unsafe { (*held).get().as_ref().cancelled() })
}

/// The free_obj of a unit class. It reclaims the unit, and an unsettled unit sends its lost outcome. A cycle that died, for example after a fatal error or a timeout, skips the outcome. The plugin then reports the worker death when the sender drops.
/// # Safety
/// `held` is the held word of the object that free_obj got, and the engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_drop(held: *mut HeldPtr) {
    guard((), || unsafe {
        // An object that new created holds no unit, because the engine refuses the private constructor after the allocation.
        let Some(ptr) = (*held).0.take() else {
            return;
        };
        let mut held = release(ptr.as_ptr());
        if !held.settled() && !(*crate::rapira_cg()).unclean_shutdown {
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
    /// The client left while the unit was queued, so the pull skips it.
    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::Cgi(ctx) => ctx.sender.as_ref().is_none_or(|tx| tx.is_closed()),
            Self::Dispatcher(held) => held.cancelled(),
        }
    }

    /// Sheds the unit when the worker cannot serve it: 503 for a CGI unit, the refusal of the plugin for a dispatcher unit.
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

    /// In dispatcher mode, returns the unit behind the object.
    pub(crate) fn into_held(self) -> Box<dyn Held> {
        match self {
            Self::Dispatcher(held) => held,
            Self::Cgi(_) => unreachable!("a CGI unit on a dispatcher intake"),
        }
    }
}

/// The pull releases both permits when it takes the unit.
pub(crate) struct Entry {
    pub(crate) unit: Unit,
    _bytes: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Saturated,
    OverBudget,
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

/// The submit end of the pool queue. Each plugin task can hold a clone.
#[derive(Clone)]
pub struct Intake {
    tx: Sender<Entry>,
    pub(crate) boot_retry: Arc<crate::start::BootRetry>,
    stats: Arc<Pool>,
    /// The bytes of the pending units.
    budget: Arc<Semaphore>,
    /// The semaphore wakes the waiting submitters in arrival order.
    slots: Arc<Semaphore>,
}

/// Counts a pending unit until the send succeeds or the submitter leaves.
struct PendingGuard<'a> {
    stats: Option<&'a Pool>,
    found_full: bool,
}

impl<'a> PendingGuard<'a> {
    fn arm(stats: &'a Pool) -> Self {
        stats.pending.fetch_add(1, Ordering::Relaxed);
        Self {
            stats: Some(stats),
            found_full: false,
        }
    }
    fn disarm(mut self) {
        self.stats = None;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Some(stats) = self.stats {
            stats.pending.fetch_sub(1, Ordering::Relaxed);
            if self.found_full {
                stats.failed_on_full_queue.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Intake {
    pub(crate) fn new(stats: Arc<Pool>) -> (Self, Receiver<Entry>) {
        let (tx, rx) = unbounded();
        (
            Self {
                tx,
                boot_retry: Arc::default(),
                stats,
                budget: Arc::new(Semaphore::new(BODY_BUDGET)),
                slots: Arc::new(Semaphore::new(INTAKE_SLOTS)),
            },
            rx,
        )
    }

    /// Reserve the body bytes and a queue slot. A body larger than the pool budget needs the full budget.
    pub async fn submit(&self, unit: Unit, bytes: usize) -> Result<(), Refused> {
        let Ok(held) =
            Arc::clone(&self.budget).try_acquire_many_owned(bytes.min(BODY_BUDGET) as u32)
        else {
            self.stats.refused.fetch_add(1, Ordering::Relaxed);
            return Err(Refused::OverBudget);
        };
        // Count the unit before a worker can take it.
        let mut pending = PendingGuard::arm(&self.stats);
        let slot = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(slot) => slot,
            Err(_) => {
                pending.found_full = true;
                let wait = Arc::clone(&self.slots).acquire_owned();
                let Ok(slot) = tokio::time::timeout(INTAKE_WAIT, wait).await else {
                    tracing::warn!(
                        target: "rapira",
                        "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                        self.stats.pending.load(Ordering::Relaxed)
                    );
                    self.stats.refused.fetch_add(1, Ordering::Relaxed);
                    return Err(Refused::Saturated);
                };
                slot.expect("the intake never closes its slot semaphore")
            }
        };
        let entry = Entry {
            unit,
            _bytes: held,
            _slot: slot,
        };
        self.tx
            .send(entry)
            .expect("workers keep the receiver until the last intake drops");
        pending.disarm();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        fn class(&self) -> *mut zend_class_entry {
            std::ptr::null_mut()
        }
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

    fn intake() -> (Intake, crossbeam_channel::Receiver<Entry>) {
        Intake::new(Arc::new(rapira_scoreboard::Pool::new("http", 2)))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn submitters_share_the_body_budget_of_the_pool() {
        struct Case {
            name: &'static str,
            queued_bytes: usize,
            next_bytes: usize,
            result: Result<(), Refused>,
            pending: u64,
            refused: u64,
        }
        let cases = [
            Case {
                name: "the next body fits the remaining byte",
                queued_bytes: BODY_BUDGET - 1,
                next_bytes: 1,
                result: Ok(()),
                pending: 2,
                refused: 0,
            },
            Case {
                name: "a large first body holds the whole budget",
                queued_bytes: BODY_BUDGET + 1,
                next_bytes: 1,
                result: Err(Refused::OverBudget),
                pending: 1,
                refused: 1,
            },
        ];
        for case in cases {
            let (intake, _rx) = intake();
            intake.submit(probe(), case.queued_bytes).await.unwrap();
            let other = intake.clone();
            let result = other.submit(probe(), case.next_bytes).await;
            assert_eq!(result, case.result, "{}", case.name);
            assert_eq!(
                (
                    intake.stats.pending.load(Ordering::Relaxed),
                    intake.stats.refused.load(Ordering::Relaxed),
                    intake.stats.failed_on_full_queue.load(Ordering::Relaxed),
                ),
                (case.pending, case.refused, 0),
                "{}",
                case.name
            );
        }
    }

    /// Each case fills all INTAKE_SLOTS slots first, so the submit under test finds the intake full. The paused clock moves to the next timer at once, so the waits take no real time.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_full_intake_counts_each_unit_that_never_enters() {
        enum During {
            Nothing,
            ClientLeaves,
            RoomFrees,
        }
        struct Counts {
            refused: u64,
            failed_on_full_queue: u64,
            /// The pending units beyond the INTAKE_SLOTS units that fill the intake.
            pending: u64,
        }
        struct Case {
            name: &'static str,
            /// What happens 1 s into the wait.
            during: During,
            /// None means that the client left, so the submit gives no result.
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
                    refused: 1,
                    failed_on_full_queue: 1,
                    pending: 0,
                },
            },
            Case {
                name: "the client leaves during the wait",
                during: During::ClientLeaves,
                result: None,
                want: Counts {
                    refused: 0,
                    failed_on_full_queue: 1,
                    pending: 0,
                },
            },
            Case {
                name: "room frees during the wait",
                during: During::RoomFrees,
                result: Some(Ok(())),
                // The unit is in the queue. Only the pull of the PHP thread subtracts it from pending.
                want: Counts {
                    refused: 0,
                    failed_on_full_queue: 0,
                    pending: 1,
                },
            },
        ];
        for case in cases {
            let (intake, rx) = intake();
            let stats = &intake.stats;
            for _ in 0..INTAKE_SLOTS {
                intake.submit(probe(), 0).await.unwrap();
            }
            let submit = intake.submit(probe(), 0);
            let result = match case.during {
                During::Nothing => Some(submit.await),
                During::ClientLeaves => tokio::time::timeout(ACTION_DELAY, submit).await.ok(),
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
                    stats.refused.load(Ordering::Relaxed),
                    stats.failed_on_full_queue.load(Ordering::Relaxed),
                    stats.pending.load(Ordering::Relaxed),
                ),
                (
                    case.want.refused,
                    case.want.failed_on_full_queue,
                    case.want.pending + INTAKE_SLOTS as u64,
                ),
                "{}: refused, failed on full queue, pending",
                case.name
            );
        }
    }
}
