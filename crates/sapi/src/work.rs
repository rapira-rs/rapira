use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use tokio::sync::Notify;

use crate::callbacks::guard;
use crate::pool::{CachePadded, Event, Pool, record};
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
        record(Event::Handled { errored: true });
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
            Self::Cgi(ctx) => ctx.is_cancelled(),
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

/// The pull releases the slot and `budget_bytes` through `IntakeLimits` when it takes the unit.
pub(crate) struct Entry {
    pub(crate) unit: Unit,
    pub(crate) budget_bytes: u32,
}

/// Lock-free body-byte budget and queue-slot accounting owned by `Pool`.
pub(crate) struct IntakeLimits {
    budget_avail: CachePadded<AtomicIsize>,
    slots_avail: CachePadded<AtomicIsize>,
    slot_waiters: CachePadded<AtomicUsize>,
    slot_notify: Notify,
}

impl IntakeLimits {
    pub(crate) fn new() -> Self {
        Self {
            budget_avail: CachePadded(AtomicIsize::new(BODY_BUDGET as isize)),
            slots_avail: CachePadded(AtomicIsize::new(INTAKE_SLOTS as isize)),
            slot_waiters: CachePadded(AtomicUsize::new(0)),
            slot_notify: Notify::new(),
        }
    }

    pub(crate) fn pending(&self) -> u64 {
        let used = (INTAKE_SLOTS as isize - self.slots_avail.0.load(Ordering::Relaxed))
            .clamp(0, INTAKE_SLOTS as isize) as u64;
        used + self.slot_waiters.0.load(Ordering::Relaxed) as u64
    }

    fn try_acquire_budget(&self, want: u32) -> bool {
        if want == 0 {
            return true;
        }
        let want = want as isize;
        if self.budget_avail.0.fetch_sub(want, Ordering::Relaxed) >= want {
            return true;
        }
        self.budget_avail.0.fetch_add(want, Ordering::Relaxed);
        false
    }

    fn release_budget(&self, bytes: u32) {
        if bytes > 0 {
            self.budget_avail
                .0
                .fetch_add(bytes as isize, Ordering::Relaxed);
        }
    }

    fn try_acquire_slot(&self) -> bool {
        if self.slots_avail.0.fetch_sub(1, Ordering::SeqCst) > 0 {
            return true;
        }
        // Restore speculative decrement; wake a waiter if a worker released concurrently.
        if self.slots_avail.0.fetch_add(1, Ordering::SeqCst) >= 0
            && self.slot_waiters.0.load(Ordering::SeqCst) > 0
        {
            self.slot_notify.notify_one();
        }
        false
    }

    async fn acquire_slot_slow(&self) {
        struct WaiterGuard<'a>(&'a IntakeLimits);
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.0.slot_waiters.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.slot_waiters.0.fetch_add(1, Ordering::SeqCst);
        let _waiter = WaiterGuard(self);
        loop {
            let notified = self.slot_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_acquire_slot() {
                return;
            }
            notified.await;
        }
    }

    /// Releases one slot and `budget_bytes` after a worker removes an entry from the channel.
    pub(crate) fn release(&self, budget_bytes: u32) {
        self.release_budget(budget_bytes);
        self.slots_avail.0.fetch_add(1, Ordering::SeqCst);
        if self.slot_waiters.0.load(Ordering::SeqCst) > 0 {
            self.slot_notify.notify_one();
        }
    }
}

/// Restores reserved body bytes if `submit` times out or is cancelled while waiting for a slot.
struct BudgetGuard<'a> {
    limits: &'a IntakeLimits,
    bytes: u32,
}

impl<'a> BudgetGuard<'a> {
    fn arm(limits: &'a IntakeLimits, bytes: u32) -> Self {
        Self { limits, bytes }
    }

    fn disarm(mut self) -> u32 {
        let bytes = self.bytes;
        self.bytes = 0;
        bytes
    }
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        self.limits.release_budget(self.bytes);
    }
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

/// The workers of one pool that wait for a unit.
#[derive(Default)]
pub(crate) struct Idle {
    /// Count of workers currently parked in `waiting.workers`. Checked lock-free after a SeqCst fence on submit so saturated pools never lock the mutex.
    idle_count: CachePadded<AtomicUsize>,
    waiting: Mutex<Waiting>,
}

#[derive(Default)]
struct Waiting {
    /// The worker that started its wait last is at the end.
    workers: Vec<Thread>,
    /// The workers that a submit woke and that are not back from their wait.
    waking: usize,
}

impl Idle {
    pub(crate) fn has_waiters(&self) -> bool {
        self.idle_count.0.load(Ordering::Relaxed) > 0
    }

    pub(crate) fn enter(&self, worker: &Thread) {
        let mut waiting = self.waiting.lock().unwrap();
        waiting.workers.push(worker.clone());
        self.idle_count
            .0
            .store(waiting.workers.len(), Ordering::Relaxed);
        drop(waiting);
        // Pairs with the SeqCst fence in Intake::submit so a submit never misses a waiter while the waiter misses the unit.
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    /// Ends the wait of `worker`.
    pub(crate) fn leave(&self, worker: &Thread) {
        let mut waiting = self.waiting.lock().unwrap();
        let at = waiting.workers.iter().rposition(|w| w.id() == worker.id());
        match at {
            Some(at) => {
                waiting.workers.remove(at);
                self.idle_count
                    .0
                    .store(waiting.workers.len(), Ordering::Relaxed);
            }
            // A wake took the worker from the list. The wake on close is not in the count.
            None => waiting.waking = waiting.waking.saturating_sub(1),
        }
    }

    /// Wakes the worker that started its wait last, unless a wake is in progress for each of the `queued` units. The caches of this worker are warm, and under a partial load the other workers stay asleep.
    pub(crate) fn wake_one(&self, queued: usize) {
        let mut waiting = self.waiting.lock().unwrap();
        if waiting.waking >= queued {
            return;
        }
        let Some(worker) = waiting.workers.pop() else {
            return;
        };
        self.idle_count
            .0
            .store(waiting.workers.len(), Ordering::Relaxed);
        waiting.waking += 1;
        drop(waiting);
        worker.unpark();
    }
}

/// Wakes every waiting worker when the last intake clone drops. Each worker then reads the closed queue.
struct WakeOnClose(Arc<Idle>);

impl Drop for WakeOnClose {
    fn drop(&mut self) {
        let mut waiting = self.0.waiting.lock().unwrap();
        let workers = std::mem::take(&mut waiting.workers);
        self.0.idle_count.0.store(0, Ordering::Relaxed);
        drop(waiting);
        for worker in workers {
            worker.unpark();
        }
    }
}

/// The pull end of the pool queue. Each worker holds a clone.
#[derive(Clone)]
pub(crate) struct Units {
    pub(crate) rx: Receiver<Entry>,
    pub(crate) idle: Arc<Idle>,
    pub(crate) pool: Arc<Pool>,
}

/// The submit end of the pool queue. Each plugin task can hold a clone.
#[derive(Clone)]
pub struct Intake {
    tx: Sender<Entry>,
    /// The field is after `tx`, so the queue is closed when the last clone wakes the waiting workers.
    idle: Arc<WakeOnClose>,
    pub(crate) boot_retry: Arc<crate::start::BootRetry>,
    pool: Arc<Pool>,
}

/// Counts a failed slow-path wait if the submitter times out or is cancelled before acquiring a slot.
struct FullQueueGuard<'a>(Option<&'a Pool>);

impl<'a> FullQueueGuard<'a> {
    fn arm(pool: &'a Pool) -> Self {
        Self(Some(pool))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for FullQueueGuard<'_> {
    fn drop(&mut self) {
        if let Some(pool) = self.0 {
            pool.failed_on_full_queue.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Intake {
    pub(crate) fn new(pool: Arc<Pool>) -> (Self, Units) {
        let (tx, rx) = bounded(INTAKE_SLOTS);
        let idle = Arc::<Idle>::default();
        (
            Self {
                tx,
                idle: Arc::new(WakeOnClose(Arc::clone(&idle))),
                boot_retry: Arc::default(),
                pool: Arc::clone(&pool),
            },
            Units { rx, idle, pool },
        )
    }

    /// Reserve the body bytes and a queue slot. A body larger than the pool budget needs the full budget.
    pub async fn submit(&self, unit: Unit, bytes: usize) -> Result<(), Refused> {
        let budget_bytes = bytes.min(BODY_BUDGET) as u32;
        if !self.pool.limits.try_acquire_budget(budget_bytes) {
            self.pool.refused.fetch_add(1, Ordering::Relaxed);
            return Err(Refused::OverBudget);
        }
        let budget = BudgetGuard::arm(&self.pool.limits, budget_bytes);
        if !self.pool.limits.try_acquire_slot() {
            let full = FullQueueGuard::arm(&self.pool);
            if tokio::time::timeout(INTAKE_WAIT, self.pool.limits.acquire_slot_slow())
                .await
                .is_err()
            {
                tracing::warn!(
                    target: "rapira",
                    "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                    self.pool.pending()
                );
                self.pool.refused.fetch_add(1, Ordering::Relaxed);
                return Err(Refused::Saturated);
            }
            full.disarm();
        }
        let entry = Entry {
            unit,
            budget_bytes: budget.disarm(),
        };
        self.tx
            .send(entry)
            .expect("workers keep the receiver until the last intake drops");
        // Pairs with the SeqCst fence in Idle::enter so saturated pools skip the Idle mutex without risking a missed wake.
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.idle.0.has_waiters() {
            self.idle.0.wake_one(self.tx.len());
        }
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
        let (responder, log) = crate::types::tests::responder(false);
        let mut ctx = crate::types::tests::context();
        ctx.responder = Some(responder);
        Unit::Cgi(Box::new(ctx)).shed();
        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["respond 503 truncated=false body="]
        );
    }

    fn intake() -> (Intake, Units) {
        Intake::new(Arc::new(Pool::new("http", 2)))
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
            let snapshot = intake.pool.snapshot();
            assert_eq!(
                (
                    snapshot.pending,
                    snapshot.refused,
                    snapshot.failed_on_full_queue,
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
                // One unit left the queue and the waiting submit took its slot.
                want: Counts {
                    refused: 0,
                    failed_on_full_queue: 0,
                    pending: 0,
                },
            },
        ];
        for case in cases {
            let (intake, rx) = intake();
            let pool = &intake.pool;
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
                        // The pull releases the slot and budget to the waiting submit.
                        let entry = rx.rx.try_recv().unwrap();
                        rx.pool.limits.release(entry.budget_bytes);
                    });
                    Some(result)
                }
            };
            assert_eq!(result, case.result, "{}", case.name);
            let snapshot = pool.snapshot();
            assert_eq!(
                (
                    snapshot.refused,
                    snapshot.failed_on_full_queue,
                    snapshot.pending,
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn saturated_or_cancelled_submit_returns_body_budget() {
        enum Outcome {
            Timeout,
            Cancel,
        }
        struct Case {
            name: &'static str,
            outcome: Outcome,
        }
        let cases = [
            Case {
                name: "timed out submit restores full body budget",
                outcome: Outcome::Timeout,
            },
            Case {
                name: "cancelled submit restores full body budget",
                outcome: Outcome::Cancel,
            },
        ];
        for case in cases {
            let (intake, rx) = intake();
            for _ in 0..INTAKE_SLOTS {
                intake.submit(probe(), 0).await.unwrap();
            }
            let submit = intake.submit(probe(), BODY_BUDGET);
            match case.outcome {
                Outcome::Timeout => {
                    assert_eq!(submit.await, Err(Refused::Saturated), "{}", case.name);
                }
                Outcome::Cancel => {
                    assert!(
                        tokio::time::timeout(Duration::from_secs(1), submit)
                            .await
                            .is_err(),
                        "{}",
                        case.name
                    );
                }
            }
            let entry = rx.rx.try_recv().unwrap();
            rx.pool.limits.release(entry.budget_bytes);
            assert_eq!(
                intake.submit(probe(), BODY_BUDGET).await,
                Ok(()),
                "{}",
                case.name
            );
        }
    }
}
