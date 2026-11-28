//! Pool work queue owning admission limits, bounded storage, LIFO idle parking, reload signaling, and boot-retry shedding.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use tokio::sync::Notify;

use crate::work::Unit;

#[cfg(test)]
mod tests;

pub(crate) const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// Number of slots in one pool queue.
pub(crate) const INTAKE_SLOTS: usize = 1024;

/// Body bytes that one pool holds for pending units.
const BODY_BUDGET: usize = 64 << 20;

/// An idle worker returns unused mimalloc pages after this wait.
const IDLE_PURGE_AFTER: Duration = Duration::from_secs(2);

#[repr(align(64))]
#[derive(Default)]
struct CachePadded<T>(T);

/// How long a pull waits for a unit.
#[derive(Clone, Copy)]
pub(crate) enum Wait {
    Forever,
    For(Duration),
    Never,
}

/// Result of pulling from a pool queue.
pub(crate) enum Pull {
    Unit(Unit),
    /// A reload ends this app instance or re-resolves the classic entrypoint.
    Reload,
    /// The app instance takes no more units because its quota ended or the intake closed.
    Closed,
    /// No unit arrived within the wait.
    NoUnit,
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

/// Snapshot of queue admission counters for `Pool::snapshot`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueueStats {
    pub pending: u64,
    pub failed_on_full_queue: u64,
    pub refused: u64,
}

struct Entry {
    unit: Unit,
    budget_bytes: u32,
}

/// Lock-free body-byte budget and queue-slot accounting.
#[repr(C)]
struct Limits {
    budget_avail: CachePadded<AtomicIsize>,
    slots_avail: CachePadded<AtomicIsize>,
    slot_waiters: CachePadded<AtomicUsize>,
    slot_notify: CachePadded<Notify>,
}

impl Limits {
    fn new() -> Self {
        Self {
            budget_avail: CachePadded(AtomicIsize::new(BODY_BUDGET as isize)),
            slots_avail: CachePadded(AtomicIsize::new(INTAKE_SLOTS as isize)),
            slot_waiters: CachePadded(AtomicUsize::new(0)),
            slot_notify: CachePadded(Notify::new()),
        }
    }

    fn pending(&self) -> u64 {
        let used = (INTAKE_SLOTS as isize - self.slots_avail.0.load(Ordering::Relaxed))
            .clamp(0, INTAKE_SLOTS as isize) as u64;
        used + self.slot_waiters.0.load(Ordering::Relaxed) as u64
    }

    fn try_acquire_budget(&self, want: u32) -> bool {
        if want == 0 {
            return true;
        }
        let want = want as isize;
        self.budget_avail
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |avail| {
                (avail >= want).then_some(avail - want)
            })
            .is_ok()
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
            self.slot_notify.0.notify_one();
        }
        false
    }

    #[cold]
    async fn acquire_slot_slow(&self) {
        struct WaiterGuard<'a>(&'a Limits);
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.0.slot_waiters.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.slot_waiters.0.fetch_add(1, Ordering::SeqCst);
        let _waiter = WaiterGuard(self);
        loop {
            let notified = self.slot_notify.0.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_acquire_slot() {
                return;
            }
            notified.await;
        }
    }

    /// Releases one slot and `budget_bytes` after a worker removes an entry from the channel.
    #[inline]
    fn release(&self, budget_bytes: u32) {
        self.release_budget(budget_bytes);
        self.slots_avail.0.fetch_add(1, Ordering::SeqCst);
        if self.slot_waiters.0.load(Ordering::SeqCst) > 0 {
            self.slot_notify.0.notify_one();
        }
    }
}

/// Restores reserved body bytes if `submit` times out or is cancelled while waiting for a slot.
struct BudgetGuard<'a> {
    limits: &'a Limits,
    bytes: u32,
}

impl<'a> BudgetGuard<'a> {
    fn arm(limits: &'a Limits, bytes: u32) -> Self {
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

/// Counts a failed slow-path wait if the submitter times out or is cancelled before acquiring a slot.
struct FullQueueGuard<'a>(Option<&'a AtomicU64>);

impl<'a> FullQueueGuard<'a> {
    fn arm(counter: &'a AtomicU64) -> Self {
        Self(Some(counter))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for FullQueueGuard<'_> {
    fn drop(&mut self) {
        if let Some(counter) = self.0 {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy)]
enum WaitKind {
    Idle,
    Shed,
}

#[derive(Default)]
struct Waiters {
    /// LIFO stack of parked idle workers (warmest worker at the end).
    idle: Vec<Thread>,
    /// Idle workers woken by submit or handoff that have not yet left their wait.
    waking: usize,
    /// Failed-boot workers waiting in `pull_to_shed`.
    shedders: Vec<Thread>,
}

/// Shared queue core for one worker pool.
/// Read-mostly control fields stay on cache line 0, isolated from every mutated counter and mutex.
#[repr(C)]
pub struct PoolQueue {
    rx: Receiver<Entry>,
    reload_gen: AtomicU64,
    failed_on_full_queue: AtomicU64,
    refused: AtomicU64,
    /// Set by `CloseOnDrop` when the last plugin submitter drops.
    intake_closed: AtomicBool,
    /// Set by `close()` when `Engine` drops to stop workers immediately.
    force_closed: AtomicBool,
    /// True while at least one worker in the pool can serve. Written only under `waiters`.
    pool_ready: AtomicBool,
    limits: Limits,
    /// Total parked threads (`idle.len() + shedders.len()`). Checked lock-free after a `SeqCst` fence on submit.
    waiter_count: CachePadded<AtomicUsize>,
    waiters: CachePadded<Mutex<Waiters>>,
}

impl PoolQueue {
    pub fn new() -> (Arc<Self>, Intake) {
        let (tx, rx) = bounded(INTAKE_SLOTS);
        let queue = Arc::new(Self {
            rx,
            reload_gen: AtomicU64::new(0),
            failed_on_full_queue: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            intake_closed: AtomicBool::new(false),
            force_closed: AtomicBool::new(false),
            pool_ready: AtomicBool::new(false),
            limits: Limits::new(),
            waiter_count: CachePadded(AtomicUsize::new(0)),
            waiters: CachePadded(Mutex::new(Waiters::default())),
        });
        let intake = Intake {
            tx,
            queue: Arc::clone(&queue),
            _close: Arc::new(CloseOnDrop(Arc::clone(&queue))),
        };
        (queue, intake)
    }

    /// Creates one worker-side pull slot bound to this queue at the current reload generation.
    pub(crate) fn worker_slot(self: &Arc<Self>) -> WorkerSlot {
        WorkerSlot {
            rx: self.rx.clone(),
            queue: Arc::clone(self),
            seen_reload: Cell::new(self.reload_gen.load(Ordering::Acquire)),
        }
    }

    pub(crate) fn pending(&self) -> u64 {
        self.limits.pending()
    }

    pub(crate) fn stats(&self) -> QueueStats {
        QueueStats {
            pending: self.limits.pending(),
            failed_on_full_queue: self.failed_on_full_queue.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }

    /// Signals every worker on this queue to reload and wakes all parked threads.
    #[cold]
    pub(crate) fn reload(&self) {
        self.reload_gen.fetch_add(1, Ordering::SeqCst);
        self.wake_all();
    }

    /// Forces every worker on this queue to stop immediately and wakes all parked threads.
    #[cold]
    pub(crate) fn close(&self) {
        self.force_closed.store(true, Ordering::SeqCst);
        self.wake_all();
    }

    /// Updates cached pool readiness under `waiters` and wakes failed-boot shedders when readiness becomes false.
    #[cold]
    pub(crate) fn sync_ready(&self, is_ready: impl FnOnce() -> bool) {
        let to_wake = {
            let waiters = self.waiters.0.lock().unwrap();
            let ready = is_ready();
            self.pool_ready.store(ready, Ordering::Release);
            if !ready && !waiters.shedders.is_empty() {
                waiters.shedders.clone()
            } else {
                Vec::new()
            }
        };
        for worker in to_wake {
            worker.unpark();
        }
    }

    #[inline]
    fn has_waiters(&self) -> bool {
        self.waiter_count.0.load(Ordering::Relaxed) > 0
    }

    fn update_waiter_count(&self, waiters: &Waiters) {
        self.waiter_count.0.store(
            waiters.idle.len() + waiters.shedders.len(),
            Ordering::Relaxed,
        );
    }

    fn enter(&self, kind: WaitKind, worker: &Thread) {
        let mut waiters = self.waiters.0.lock().unwrap();
        match kind {
            WaitKind::Idle => waiters.idle.push(worker.clone()),
            WaitKind::Shed => waiters.shedders.push(worker.clone()),
        }
        self.update_waiter_count(&waiters);
        drop(waiters);
        // Pairs with the SeqCst fence in Intake::submit so a submit never misses a waiter while the waiter misses the unit.
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    fn leave(&self, kind: WaitKind, worker: &Thread) {
        let mut waiters = self.waiters.0.lock().unwrap();
        let list = match kind {
            WaitKind::Idle => &mut waiters.idle,
            WaitKind::Shed => &mut waiters.shedders,
        };
        match list.iter().rposition(|w| w.id() == worker.id()) {
            Some(at) => {
                list.remove(at);
                self.update_waiter_count(&waiters);
            }
            None if matches!(kind, WaitKind::Idle) => {
                waiters.waking = waiters.waking.saturating_sub(1);
            }
            None => {}
        }
    }

    /// Wakes the warmest idle worker unless enough wakes are in flight; if no worker can serve, wakes a shedder.
    fn wake_for_unit(&self, queued: usize) {
        let target = {
            let mut waiters = self.waiters.0.lock().unwrap();
            if waiters.waking < queued
                && let Some(worker) = waiters.idle.pop()
            {
                waiters.waking += 1;
                self.update_waiter_count(&waiters);
                Some(worker)
            } else if !self.pool_ready.load(Ordering::Acquire) {
                waiters.shedders.last().cloned()
            } else {
                None
            }
        };
        if let Some(worker) = target {
            worker.unpark();
        }
    }

    #[cold]
    fn wake_all(&self) {
        let to_wake = {
            let waiters = self.waiters.0.lock().unwrap();
            let mut list = Vec::with_capacity(waiters.idle.len() + waiters.shedders.len());
            list.extend(waiters.idle.iter().cloned());
            list.extend(waiters.shedders.iter().cloned());
            list
        };
        for worker in to_wake {
            worker.unpark();
        }
    }
}

struct CloseOnDrop(Arc<PoolQueue>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.intake_closed.store(true, Ordering::SeqCst);
        self.0.wake_all();
    }
}

/// Submit end of a pool queue. Each plugin task can hold a clone.
#[derive(Clone)]
pub struct Intake {
    tx: Sender<Entry>,
    queue: Arc<PoolQueue>,
    /// Dropped after `tx`, so the channel disconnects before waking waiters.
    _close: Arc<CloseOnDrop>,
}

impl Intake {
    /// Reserves body bytes and a queue slot, then submits `unit`. A body larger than `BODY_BUDGET` reserves the full budget.
    pub async fn submit(&self, unit: Unit, bytes: usize) -> Result<(), Refused> {
        let queue = &self.queue;
        let budget_bytes = bytes.min(BODY_BUDGET) as u32;
        if !queue.limits.try_acquire_budget(budget_bytes) {
            queue.refused.fetch_add(1, Ordering::Relaxed);
            return Err(Refused::OverBudget);
        }
        let budget = BudgetGuard::arm(&queue.limits, budget_bytes);
        if !queue.limits.try_acquire_slot() {
            let full = FullQueueGuard::arm(&queue.failed_on_full_queue);
            if tokio::time::timeout(INTAKE_WAIT, queue.limits.acquire_slot_slow())
                .await
                .is_err()
            {
                tracing::warn!(
                    target: "rapira",
                    "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                    queue.pending()
                );
                queue.refused.fetch_add(1, Ordering::Relaxed);
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
        // Pairs with the SeqCst fence in enter so saturated pools skip the mutex without risking a missed wake.
        std::sync::atomic::fence(Ordering::SeqCst);
        if queue.has_waiters() {
            queue.wake_for_unit(self.tx.len());
        }
        Ok(())
    }
}

/// Worker-side pull end bound to one `PoolQueue`.
pub(crate) struct WorkerSlot {
    rx: Receiver<Entry>,
    queue: Arc<PoolQueue>,
    seen_reload: Cell<u64>,
}

impl WorkerSlot {
    #[inline]
    fn try_recv_unit(&self) -> Result<Unit, TryRecvError> {
        self.rx.try_recv().map(|entry| {
            self.queue.limits.release(entry.budget_bytes);
            entry.unit
        })
    }

    /// Checks control state, then takes a queued unit if ready. When `SHED` is true, pulls units only while no worker is ready.
    #[inline]
    fn take<const SHED: bool>(&self) -> Option<Pull> {
        if self.queue.force_closed.load(Ordering::Acquire)
            || (SHED && self.queue.intake_closed.load(Ordering::Acquire))
        {
            return Some(Pull::Closed);
        }
        let reload = self.queue.reload_gen.load(Ordering::Acquire);
        if reload > self.seen_reload.get() {
            self.seen_reload.set(reload);
            return Some(Pull::Reload);
        }
        if SHED && self.queue.pool_ready.load(Ordering::Acquire) {
            return None;
        }
        match self.try_recv_unit() {
            Ok(unit) => Some(Pull::Unit(unit)),
            Err(TryRecvError::Disconnected) => Some(Pull::Closed),
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Hands off remaining queued units to another parked worker (including when this worker took `Reload`).
    #[inline]
    fn handoff_remaining(&self) {
        if self.queue.has_waiters() {
            let rem = self.rx.len();
            if rem > 0 {
                self.queue.wake_for_unit(rem);
            }
        }
    }

    /// Pulls the next event, waiting up to `timeout` and collecting unused mimalloc pages after `IDLE_PURGE_AFTER`.
    #[inline]
    pub(crate) fn recv(&self, timeout: Duration) -> Pull {
        if let Some(pull) = self.take::<false>() {
            self.handoff_remaining();
            return pull;
        }
        if timeout.is_zero() {
            return Pull::NoUnit;
        }
        idle_wait(timeout, |t| {
            self.wait_loop::<false>(Instant::now().checked_add(t))
        })
    }

    /// Waits during a failed-boot backoff window, pulling queued units to shed only while no peer worker is ready.
    #[cold]
    pub(crate) fn pull_to_shed(&self, deadline: Instant) -> Pull {
        if let Some(pull) = self.take::<true>() {
            return pull;
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Pull::NoUnit;
        }
        idle_wait(rem, |t| {
            self.wait_loop::<true>(Some(deadline.min(Instant::now() + t)))
        })
    }

    #[cold]
    fn wait_loop<const SHED: bool>(&self, deadline: Option<Instant>) -> Pull {
        let kind = if SHED { WaitKind::Shed } else { WaitKind::Idle };
        let me = thread::current();
        loop {
            self.queue.enter(kind, &me);
            // A unit, reload, or readiness change can arrive between the last check and entering the waiter list.
            let mut got = self.take::<SHED>();
            let mut expired = false;
            if got.is_none() {
                match deadline {
                    None => thread::park(),
                    Some(until) => {
                        let left = until.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            expired = true;
                        } else {
                            thread::park_timeout(left);
                            expired = Instant::now() >= until;
                        }
                    }
                }
            }
            self.queue.leave(kind, &me);
            if got.is_none() {
                got = self.take::<SHED>();
            }
            match got {
                Some(pull) => {
                    if !SHED {
                        self.handoff_remaining();
                    }
                    return pull;
                }
                None if expired => return Pull::NoUnit,
                None => {}
            }
        }
    }
}

fn idle_wait(timeout: Duration, select: impl Fn(Duration) -> Pull) -> Pull {
    let first = timeout.min(IDLE_PURGE_AFTER);
    let got = select(first);
    if matches!(got, Pull::NoUnit) && first == IDLE_PURGE_AFTER {
        // SAFETY: mimalloc can collect on the thread that owns the idle heap.
        unsafe { libmimalloc_sys::mi_collect(true) };
        if timeout > first {
            return select(timeout - first);
        }
    }
    got
}
