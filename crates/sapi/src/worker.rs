//! One per-worker runtime owning the Cycle, Pull, quota, OPcache reload state, and active Dispatcher Unit.
//!
//! `Worker<P>` is generic over an internal `PhpRuntime` seam (`LivePhp` in production, `FakePhp` in tests)
//! so production monomorphizes with zero dynamic dispatch while multi-cycle, quota, boot-retry, and OPcache
//! coordination run in-process without PHP or thread-local test setup.

use std::cell::Cell;
use std::ffi::{CStr, c_int};
use std::path::Path;
use std::ptr::{NonNull, null, null_mut};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use crate::callbacks::{fatal_error_recorded, guard};
use crate::context::{
    bind_server_context, clean_realpath_cache, ctx, populate_request_context, resolve_script,
    script, unbind_server_context,
};
use crate::diagnostics::{Severity, error_type_to_level};
use crate::pool::{Event, Pool};
use crate::reload::OpcacheCoordinator;
use crate::start::{BootRetry, OPCACHE_POLL, Pull, UnitRx, Wait, idle_wait, run_script};
use crate::types::Context;
use crate::work::{Held, HeldPtr};
use crate::{
    E_CORE, FAILURE, RAPIRA_BAILOUT, RAPIRA_HANDLE_CONTINUE, RAPIRA_HANDLE_RECYCLE,
    RAPIRA_HANDLE_STOP, RAPIRA_OK, RAPIRA_THROW, SUCCESS, php_request_startup,
    rapira_ce_closed_exception, rapira_ce_timeout_exception, rapira_clear_last_error,
    rapira_current_plugin, rapira_eg, rapira_finish_output, rapira_opcache_reset,
    rapira_opcache_state, rapira_pg, rapira_receive_timed, rapira_receive_untimed,
    rapira_release_temporary_streams, rapira_request_activate, rapira_request_shutdown,
    rapira_request_teardown, rapira_run_handler, rapira_sg, rapira_stash_boot_shutdown_functions,
    rapira_unit_object, zend, zend_fcall_info, zend_fcall_info_cache, zend_is_auto_global_str,
    zend_long, zval,
};

const CLOSED_MSG: &CStr = c"no more work for this app instance";
const TIMEOUT_MSG: &CStr = c"no work became available within the timeout";

/// Flags returned by `rapira_opcache_state()`.
const OPCACHE_ON: c_int = 1;
const OPCACHE_PENDING: c_int = 2;
const OPCACHE_RESTARTING: c_int = 4;

/// Initial wait after a failed boot; doubles per consecutive failure up to `BOOT_RETRY_MAX`.
const BOOT_RETRY_BASE: Duration = Duration::from_millis(500);
/// Longest wait between two failed boots so `/readyz`-gated pools recover without incoming requests.
const BOOT_RETRY_MAX: Duration = Duration::from_secs(5);

/// How the current app instance ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The intake closed. The worker stops.
    Stop,
    /// The worker starts a new app instance.
    Recycle,
}

/// What an app boot does about OPcache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpcacheBoot {
    Run,
    /// OPcache is on and a restart is pending. This app instance holds the usage lock, so it leaves at its first pull.
    LeaveAtFirstPull,
    /// OPcache is off for this app instance. It leaves when `last_restart_time` is no longer `seen`.
    Wait {
        seen: zend_long,
    },
}

enum RecvMode {
    Wait(i64),
    Try,
}

/// Outcome of `Worker::receive_dispatcher` before translating into PHP return values or exceptions.
enum ReceiveOutcome {
    Unit(*mut dyn Held),
    Busy,
    Closed,
    TimedOut,
    Empty,
}

/// Internal seam separating `Worker` lifecycle logic from live Zend/SAPI FFI calls.
pub(crate) trait PhpRuntime: Sized {
    fn prepare_entrypoint(&self, script: &Path);
    /// Runs a standalone PHP request calling `opcache_reset()`. Returns `(started, clean_shutdown)`.
    fn run_opcache_reset_request(&self, worker: &Worker<Self>) -> (bool, bool);
    /// Reads `(state, last_restart_time)` from `rapira_opcache_state()`.
    fn opcache_state(&self) -> (c_int, zend_long);
    fn boot_request_startup(&self) -> bool;
    /// Runs the entrypoint script while `worker` is active for re-entrant PHP callbacks.
    fn execute_entrypoint(worker: &Worker<Self>);
    /// Tears down the active request. Returns true when a bailout occurred.
    fn teardown_request(&self) -> bool;
    fn stash_boot_shutdown_functions(&self);
    /// Shuts down the active request. Returns true when a bailout occurred.
    fn shutdown_request(&self) -> bool;
    fn log_and_clear_last_error(&self);
    fn disarm_timer(&self);
    fn arm_timer(&self);
    /// Runs one Classic-mode request for `ctx`. Returns true when the request errored.
    fn run_classic_unit(&self, ctx: &mut Context) -> bool;
    /// Runs one Worker-mode handler invocation. Returns `(outcome, flushed, teardown)`.
    fn run_worker_handler(
        &self,
        ctx: &mut Context,
        fci: *mut zend_fcall_info,
        fcc: *mut zend_fcall_info_cache,
    ) -> (c_int, c_int, c_int);
    /// Exits the process when consecutive boot failures hit the limit before the pool served.
    fn exit_boot_failed(&self, code: i32) -> bool;
    /// Waits for a unit to shed during a failed-boot backoff window.
    fn pull_to_shed(
        &self,
        worker: &Worker<Self>,
        retry_at: Instant,
        retry: &Weak<BootRetry>,
    ) -> Pull;
}

/// Zero-sized production adapter backed by live Zend/SAPI calls.
pub(crate) struct LivePhp;

thread_local! {
    /// Scoped pointer to the stack-owned `Worker<LivePhp>` on this PHP thread, active during worker execution.
    static ACTIVE_WORKER: Cell<*const Worker<LivePhp>> = const { Cell::new(null()) };
}

struct ActiveWorkerGuard(*const Worker<LivePhp>);

impl ActiveWorkerGuard {
    fn enter(worker: &Worker<LivePhp>) -> Self {
        let prev = ACTIVE_WORKER.replace(std::ptr::from_ref(worker));
        Self(prev)
    }
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        ACTIVE_WORKER.set(self.0);
    }
}

/// Runs `f` against the active `Worker<LivePhp>` on this thread, if one is bound.
#[inline]
fn with_active<R>(f: impl FnOnce(&Worker<LivePhp>) -> R) -> Option<R> {
    let ptr = ACTIVE_WORKER.get();
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ActiveWorkerGuard` keeps `ptr` valid for the entire worker run on this thread,
    // and `Worker` mutates only `Cell` fields, so shared references are sound across re-entrant callbacks.
    Some(f(unsafe { &*ptr }))
}

/// Returns the current cycle number of this PHP worker thread (1 in the first cycle, 0 outside a worker).
pub(crate) fn current_cycle() -> u64 {
    with_active(Worker::cycle).unwrap_or(0)
}

/// Clears the active Dispatcher unit slot when `ptr` is reclaimed and accounts for its quota once.
pub(crate) fn forget_held(ptr: *const ()) {
    let _ = with_active(|w| w.forget_held(ptr));
}

/// Per-worker state machine owning Cycle, Pull, quota, OPcache wait/reload, and active Dispatcher Unit.
pub(crate) struct Worker<P: PhpRuntime> {
    units: UnitRx,
    pool: Arc<Pool>,
    index: usize,
    opcache: Arc<OpcacheCoordinator>,
    max_requests: u64,
    php: P,
    /// Cycle counter on this worker thread (1 in the first cycle).
    cycle: Cell<u64>,
    /// Served units in the current app instance.
    served: Cell<u64>,
    /// True once the current app instance pulled at least one unit.
    received: Cell<bool>,
    /// Set once the current app instance should stop taking units.
    end: Cell<Option<End>>,
    /// True until the first `handle_request()` call tears down the boot request.
    first_call: Cell<bool>,
    /// Set when the app booted with OPcache off; holds the `last_restart_time` to watch.
    opcache_wait: Cell<Option<zend_long>>,
    /// `last_restart_time` at the end of the previous app instance on this worker.
    previous_end: Cell<Option<zend_long>>,
    /// Restart reason saved by `opcache_restart_scheduled` while PHP holds the OPcache lock.
    opcache_scheduled: Cell<Option<c_int>>,
    /// True while this thread runs a host-initiated `opcache_reset()` request.
    host_reset: Cell<bool>,
    /// Last Dispatcher unit handed to PHP, kept so bailouts can reclaim it.
    current_unit: Cell<Option<*mut dyn Held>>,
    /// True once `current_unit` has been counted against `served` / `max_requests`.
    current_unit_counted: Cell<bool>,
}

impl<P: PhpRuntime> Worker<P> {
    pub(crate) fn new(
        units: UnitRx,
        pool: Arc<Pool>,
        index: usize,
        opcache: Arc<OpcacheCoordinator>,
        max_requests: u64,
        php: P,
    ) -> Self {
        Self {
            units,
            pool,
            index,
            opcache,
            max_requests: if max_requests == 0 {
                u64::MAX
            } else {
                max_requests
            },
            php,
            cycle: Cell::new(0),
            served: Cell::new(0),
            received: Cell::new(false),
            end: Cell::new(None),
            first_call: Cell::new(true),
            opcache_wait: Cell::new(None),
            previous_end: Cell::new(None),
            opcache_scheduled: Cell::new(None),
            host_reset: Cell::new(false),
            current_unit: Cell::new(None),
            current_unit_counted: Cell::new(true),
        }
    }

    #[inline]
    pub(crate) fn cycle(&self) -> u64 {
        self.cycle.get()
    }

    fn renew_quota(&self) {
        self.served.set(0);
        self.received.set(false);
        self.end.set(None);
    }

    fn tick_quota(&self) {
        let served = self.served.get() + 1;
        self.served.set(served);
        if served == self.max_requests {
            info!(target: "rapira", "app served {served} requests; restarting the app");
            self.mark_end(End::Recycle);
        }
    }

    /// Ends the current app instance. `End::Stop` overrides `End::Recycle`.
    fn mark_end(&self, end: End) {
        if end == End::Stop || self.end.get().is_none() {
            self.end.set(Some(end));
        }
    }

    fn ended(&self) -> bool {
        self.end.get().is_some()
    }

    /// How the app instance ended. An instance that took a unit recycles; `None` is a failed boot.
    fn outcome(&self) -> Option<End> {
        self.end
            .get()
            .or_else(|| self.received.get().then_some(End::Recycle))
    }

    /// Clears the Dispatcher unit slot if it still points at `ptr` and counts its quota once.
    pub(crate) fn forget_held(&self, ptr: *const ()) {
        if self
            .current_unit
            .get()
            .is_some_and(|u| std::ptr::addr_eq(u, ptr))
        {
            self.current_unit.set(None);
            if !self.current_unit_counted.replace(true) {
                self.tick_quota();
            }
        }
    }

    /// Reclaims a Dispatcher unit that `free_obj` never saw (for example after a shutdown or OOM bailout).
    fn reclaim_current(&self) {
        if let Some(ptr) = self.current_unit.take() {
            if !self.current_unit_counted.replace(true) {
                self.tick_quota();
            }
            // SAFETY: `ptr` came from `Box::into_raw` in `receive_dispatcher` and `current_unit` owned it.
            let st = unsafe { Box::from_raw(ptr) };
            if !st.settled() {
                self.pool
                    .record_worker(self.index, Event::Handled { errored: true });
            }
        }
    }

    fn has_active_unit(&self) -> bool {
        self.current_unit
            .get()
            .is_some_and(|ptr| unsafe { !(*ptr).is_finalized() })
    }

    fn with_host_reset(&self, f: impl FnOnce()) {
        self.host_reset.set(true);
        f();
        self.host_reset.set(false);
    }

    fn on_opcache_restart_scheduled(&self, reason: c_int) {
        if !self.host_reset.get() {
            self.opcache_scheduled.set(Some(reason));
        }
    }

    /// Wakes all workers after PHP releases the OPcache lock so a scheduled OPcache restart can run.
    fn reload_for_opcache_restart(&self) {
        let Some(reason) = self.opcache_scheduled.take() else {
            return;
        };
        self.units.wake_workers();
        // https://github.com/php/php-src/blob/php-8.6.0/ext/opcache/ZendAccelerator.h#L105-L109
        let why = match reason {
            0 => "out of memory",
            1 => "hash table full",
            _ => "opcache_reset()",
        };
        warn!(target: "rapira", "OPcache scheduled a restart ({why}); every app restarts so that the restart can run");
    }

    /// Claims and runs a host-requested `opcache_reset()` before loading a script.
    #[must_use = "start the app or run the unit only after the reset request succeeds"]
    fn reset_opcache_before_request(&self) -> bool {
        let Some(sent) = self.opcache.claim_reset() else {
            return true;
        };
        let (started, clean_shutdown) = self.php.run_opcache_reset_request(self);
        self.opcache.finish_reset(sent, started);
        self.reload_for_opcache_restart();
        started && clean_shutdown
    }

    /// Reads OPcache state once at app boot (requires an active PHP request).
    fn note_opcache_at_boot(&self) {
        let (state, last_restart) = self.php.opcache_state();
        self.opcache_wait.set(None);
        match opcache_boot(state, last_restart, self.previous_end.get()) {
            OpcacheBoot::Run => {}
            OpcacheBoot::LeaveAtFirstPull => self.mark_end(End::Recycle),
            OpcacheBoot::Wait { seen } => {
                info!(target: "rapira", "the app booted with OPcache off; it restarts when the OPcache restart has run");
                self.opcache_wait.set(Some(seen));
            }
        }
    }

    /// True when the app booted with OPcache off and the pending restart has now finished (or state read failed).
    fn opcache_restart_ran(&self) -> bool {
        self.opcache_wait.get().is_some_and(|seen| {
            let (state, last_restart) = self.php.opcache_state();
            // A read fails here only at the memory limit, and the app instance must end then.
            state < 0 || last_restart != seen
        })
    }

    /// Keeps the worker idle across the pull and transitions it back to active before returning to PHP.
    fn pull_with(&self, select: impl FnOnce(&Self) -> Pull) -> Pull {
        self.reload_for_opcache_restart();
        if self.ended() {
            return Pull::Closed;
        }
        self.pool.record_worker(self.index, Event::Idle);
        let got = select(self);
        self.pool.record_worker(self.index, Event::Active);
        got
    }

    /// Pulls the next ready event, skipping cancelled units and keeping a single deadline across OPcache polls.
    fn next_pull(&self, wait: Wait) -> Pull {
        let deadline = match wait {
            Wait::For(t) => Some(Instant::now() + t),
            _ => None,
        };
        loop {
            if self.opcache_restart_ran() {
                info!(target: "rapira", "the OPcache restart has run; restarting the app");
                self.mark_end(End::Recycle);
                return Pull::Closed;
            }
            let got = if matches!(wait, Wait::Never) {
                self.units.recv(&self.units.rx, Duration::ZERO)
            } else {
                let left = deadline.map_or(Duration::MAX, |d| {
                    d.saturating_duration_since(Instant::now())
                });
                let timeout = if self.opcache_wait.get().is_some() {
                    left.min(OPCACHE_POLL)
                } else {
                    left
                };
                idle_wait(timeout, |t| self.units.recv(&self.units.rx, t))
            };
            match &got {
                Pull::Unit(unit) if unit.cancelled() => {
                    self.pool.record_worker(self.index, Event::Shed);
                    continue;
                }
                Pull::Unit(_) => self.received.set(true),
                Pull::Reload => self.mark_end(End::Recycle),
                Pull::Closed => self.mark_end(End::Stop),
                Pull::NoUnit
                    if !matches!(wait, Wait::Never)
                        && deadline.is_none_or(|d| Instant::now() < d) =>
                {
                    continue;
                }
                Pull::NoUnit => {}
            }
            return got;
        }
    }

    /// Probes non-blockingly first while keeping the execution timer armed; disarms only before parking or stopping.
    fn pull_timed(&self, wait: Wait) -> Pull {
        let mut disarmed = false;
        let got = self.pull_with(|w| {
            if !matches!(wait, Wait::Never) {
                let got = w.next_pull(Wait::Never);
                if !matches!(got, Pull::NoUnit) {
                    return got;
                }
            }
            w.php.disarm_timer();
            disarmed = true;
            w.next_pull(wait)
        });
        if !matches!(got, Pull::Unit(_)) && !disarmed {
            self.php.disarm_timer();
        }
        got
    }

    /// Classic mode: resolves the entrypoint and executes it once per pulled CGI unit.
    pub(crate) fn run_classic(&self, entrypoint: &Path) {
        loop {
            self.php.prepare_entrypoint(entrypoint);
            loop {
                match self.pull_with(|w| w.next_pull(Wait::Forever)) {
                    Pull::Unit(unit) => {
                        let mut ctx = unit.into_cgi();
                        let errored = if !self.reset_opcache_before_request() {
                            error!(target: "rapira", "OPcache reset request failed; refusing the request");
                            ctx.refuse(500);
                            true
                        } else {
                            self.php.run_classic_unit(&mut ctx)
                        };
                        self.pool
                            .record_worker(self.index, Event::Handled { errored });
                    }
                    Pull::Reload => {
                        self.pool.record_worker(self.index, Event::Starting);
                        self.renew_quota();
                        info!(target: "rapira", "reload requested; resolving the entrypoint again");
                        break;
                    }
                    Pull::Closed | Pull::NoUnit => return,
                }
            }
        }
    }

    /// Runs one resident app cycle (Worker or Dispatcher mode) and returns how it ended (`None` = failed boot).
    fn run_cycle(&self, entrypoint: &Path) -> Option<End> {
        self.renew_quota();
        self.php.prepare_entrypoint(entrypoint);
        if !self.reset_opcache_before_request() {
            error!(target: "rapira", "OPcache reset request failed; restarting the app");
            self.reclaim_current();
            return self.outcome();
        }
        self.cycle.set(self.cycle.get() + 1);
        let started = self.php.boot_request_startup();
        if started {
            self.note_opcache_at_boot();
            P::execute_entrypoint(self);
            // The boot of the next app instance compares `last_restart_time` with this value.
            let (state, last_restart) = self.php.opcache_state();
            self.previous_end.set((state >= 0).then_some(last_restart));
        } else {
            error!(target: "rapira", "php_request_startup() failed");
        }

        self.first_call.set(true);
        self.php.log_and_clear_last_error();
        let bailed = self.php.shutdown_request();
        // A script or its shutdown functions can schedule a restart before leaving.
        self.reload_for_opcache_restart();
        if bailed {
            error!(target: "rapira", "php_request_shutdown() bailed; restarting the app");
        }
        self.reclaim_current();
        self.outcome()
    }

    /// Resident loop (Worker and Dispatcher modes) with recycle, boot-failure backoff, and unit shedding.
    pub(crate) fn run_resident(
        &self,
        entrypoint: &Path,
        max_boot_failures: u32,
        retry: &Weak<BootRetry>,
    ) {
        let mut failures: u32 = 0;
        let mut retry_delay = BOOT_RETRY_BASE;
        'cycles: loop {
            let recycled = match self.run_cycle(entrypoint) {
                Some(End::Stop) => break,
                Some(End::Recycle) => true,
                None => false,
            };
            let was_serving = self.pool.state(self.index).serving();
            self.pool.record_worker(
                self.index,
                if recycled {
                    Event::Recycled
                } else {
                    Event::Starting
                },
            );
            if was_serving && let Some(retry) = retry.upgrade() {
                retry.wake();
            }
            if recycled {
                failures = 0;
                retry_delay = BOOT_RETRY_BASE;
                continue;
            }
            failures = failures.saturating_add(1);
            if give_up_boot(failures, max_boot_failures, self.pool.served())
                && !self.pool.draining()
            {
                error!(
                    target: "rapira",
                    "{} pool: worker {} failed to boot {failures} times; the pool never served; exiting with code 70",
                    self.pool.name,
                    self.index
                );
                if self.php.exit_boot_failed(70) {
                    break;
                }
            }
            // Units that arrive during the wait do not change the next boot time.
            let retry_at = Instant::now() + retry_delay;
            retry_delay = (retry_delay * 2).min(BOOT_RETRY_MAX);
            loop {
                match self.php.pull_to_shed(self, retry_at, retry) {
                    Pull::Closed => break 'cycles,
                    Pull::Unit(unit) => {
                        unit.shed();
                        self.pool.record_worker(self.index, Event::Shed);
                    }
                    Pull::Reload | Pull::NoUnit => break,
                }
            }
        }
    }

    /// Worker-mode `handle_request()` core: tears down boot request on first call, pulls one CGI unit, runs handler, and counts quota.
    fn handle_request(&self, fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> u32 {
        if self.first_call.replace(false) {
            if self.php.teardown_request() {
                error!(target: "rapira", "rapira_request_teardown() bailed on first call; recycling");
                self.mark_end(End::Recycle);
                return RAPIRA_HANDLE_RECYCLE;
            }
            // Hide boot shutdown functions from the per-unit pass; they run at cycle end.
            self.php.stash_boot_shutdown_functions();
        }
        self.php.log_and_clear_last_error();
        let mut ctx = match self.pull_timed(Wait::Forever) {
            Pull::Unit(unit) => unit.into_cgi(),
            Pull::Closed | Pull::Reload | Pull::NoUnit => return RAPIRA_HANDLE_STOP,
        };

        let (outcome, flushed, teardown) = self.php.run_worker_handler(&mut ctx, fci, fcc);

        let recycle = [outcome, flushed, teardown].contains(&(RAPIRA_BAILOUT as c_int));
        let errored = recycle || [outcome, teardown].contains(&(RAPIRA_THROW as c_int));

        self.php.log_and_clear_last_error();
        self.pool
            .record_worker(self.index, Event::Handled { errored });
        self.tick_quota();
        if recycle {
            self.mark_end(End::Recycle);
            RAPIRA_HANDLE_RECYCLE
        } else {
            RAPIRA_HANDLE_CONTINUE
        }
    }

    /// Dispatcher-mode `receive()` / `tryReceive()` core: checks prior unit, counts quota, pulls next unit, and tracks the slot.
    fn receive_dispatcher(&self, mode: RecvMode) -> ReceiveOutcome {
        if let Some(ptr) = self.current_unit.get() {
            // SAFETY: `current_unit` points to a leaked `Held` box owned until `forget_held` clears the slot.
            let held = unsafe { &mut *ptr };
            if !held.settled() && held.cancelled() {
                tracing::debug!(
                    target: "rapira",
                    "receive() discarded an unfinalized unit whose client left"
                );
                held.discard();
            }
            if !held.settled() {
                return ReceiveOutcome::Busy;
            }
            if !self.current_unit_counted.replace(true) {
                self.tick_quota();
            }
        }
        let wait = match mode {
            RecvMode::Try | RecvMode::Wait(0) => Wait::Never,
            RecvMode::Wait(-1) => Wait::Forever,
            RecvMode::Wait(t) => Wait::For(Duration::from_micros(t as u64)),
        };
        match self.pull_timed(wait) {
            Pull::Unit(unit) => {
                self.php.arm_timer();
                let ptr = Box::into_raw(unit.into_held());
                self.current_unit.set(Some(ptr));
                self.current_unit_counted.set(false);
                ReceiveOutcome::Unit(ptr)
            }
            Pull::Closed | Pull::Reload => ReceiveOutcome::Closed,
            Pull::NoUnit if matches!(mode, RecvMode::Try) => ReceiveOutcome::Empty,
            Pull::NoUnit => ReceiveOutcome::TimedOut,
        }
    }
}

impl Worker<LivePhp> {
    /// Runs Classic mode with `ACTIVE_WORKER` bound for FFI callbacks.
    pub(crate) fn run_classic_live(&self, entrypoint: &Path) {
        let _active = ActiveWorkerGuard::enter(self);
        self.run_classic(entrypoint);
    }

    /// Runs Worker or Dispatcher mode with `ACTIVE_WORKER` bound for FFI callbacks.
    pub(crate) fn run_resident_live(
        &self,
        entrypoint: &Path,
        max_boot_failures: u32,
        retry: &Weak<BootRetry>,
    ) {
        let _active = ActiveWorkerGuard::enter(self);
        self.run_resident(entrypoint, max_boot_failures, retry);
    }
}

impl PhpRuntime for LivePhp {
    fn prepare_entrypoint(&self, entrypoint: &Path) {
        resolve_script(entrypoint);
        clean_realpath_cache();
    }

    fn run_opcache_reset_request(&self, worker: &Worker<Self>) -> (bool, bool) {
        let started = unsafe { php_request_startup() } == SUCCESS;
        if started {
            worker.with_host_reset(|| unsafe { rapira_opcache_reset() });
        }
        let shutdown = unsafe { rapira_request_shutdown() };
        (started, shutdown != RAPIRA_BAILOUT as c_int)
    }

    fn opcache_state(&self) -> (c_int, zend_long) {
        let mut last_restart: zend_long = 0;
        let state = unsafe { rapira_opcache_state(&mut last_restart) };
        (state, last_restart)
    }

    /// Starts the boot request with `argv` of `php entrypoint.php` and builds `$_SERVER`, matching CLI.
    /// After this, `argc` is 0 again so later `$_SERVER` builds do not reference a modified `$argv`.
    /// https://github.com/php/php-src/blob/php-8.5.11/sapi/cli/php_cli.c#L941
    /// https://github.com/php/php-src/blob/php-8.5.11/main/php_variables.c#L881-L887
    fn boot_request_startup(&self) -> bool {
        let mut argv = [script().filename().as_ptr().cast_mut()];
        unsafe {
            let sg = rapira_sg();
            (*sg).request_info.argc = 1;
            (*sg).request_info.argv = argv.as_mut_ptr();
            // PHP 8.4 adds argv only with register_argc_argv on, matching embed SAPI.
            // https://github.com/php/php-src/blob/php-8.4.26/sapi/embed/php_embed.c#L26-L32
            #[cfg(php84)]
            let register_argc_argv =
                std::ptr::replace(&raw mut (*rapira_pg()).register_argc_argv, true);
            let started = php_request_startup() == SUCCESS;
            if started {
                zend_is_auto_global_str(c"_SERVER".as_ptr(), c"_SERVER".count_bytes());
            }
            #[cfg(php84)]
            {
                (*rapira_pg()).register_argc_argv = register_argc_argv;
            }
            (*sg).request_info.argc = 0;
            (*sg).request_info.argv = null_mut();
            started
        }
    }

    fn execute_entrypoint(_worker: &Worker<Self>) {
        unsafe { run_script(script().filename()) };
    }

    fn teardown_request(&self) -> bool {
        unsafe { rapira_request_teardown() == RAPIRA_BAILOUT as c_int }
    }

    fn stash_boot_shutdown_functions(&self) {
        unsafe { rapira_stash_boot_shutdown_functions() };
    }

    fn shutdown_request(&self) -> bool {
        unsafe { rapira_request_shutdown() == RAPIRA_BAILOUT as c_int }
    }

    /// Logs the last unmasked PHP diagnostic that `php_error_cb` did not log, then clears it.
    fn log_and_clear_last_error(&self) {
        unsafe {
            let pg = rapira_pg();
            let msg = (*pg).last_error_message;
            let mask = (*rapira_eg()).error_reporting;
            let php_logged = (*pg).last_error_type & (mask | E_CORE as c_int) != 0
                && (*pg).log_errors
                && (*pg).error_log.is_null();
            if !msg.is_null() && !php_logged {
                let lineno = (*pg).last_error_lineno;
                let Severity { level, label } = error_type_to_level((*pg).last_error_type, mask);
                crate::diagnostics::event_at!(
                    "php",
                    level,
                    "{label}: {} in {}:{}",
                    String::from_utf8_lossy(zend::zstr_bytes(msg)),
                    String::from_utf8_lossy(zend::zstr_bytes((*pg).last_error_file)),
                    lineno
                );
            }
            rapira_clear_last_error();
        }
    }

    fn disarm_timer(&self) {
        unsafe { rapira_receive_untimed() };
    }

    fn arm_timer(&self) {
        unsafe { rapira_receive_timed() };
    }

    // `run_script` also returns false for exit() and die(), so only a recorded fatal error marks a failure.
    fn run_classic_unit(&self, ctx: &mut Context) -> bool {
        bind_server_context(ctx);
        let errored = unsafe {
            populate_request_context(ctx);
            if php_request_startup() == FAILURE {
                // Refuse before shutdown so `php_header()` in RSHUTDOWN does not commit a 200 head first.
                ctx.refuse(500);
                rapira_request_shutdown();
                unbind_server_context();
                return true;
            }
            crate::context::apply_proto_num(ctx);

            let failed = !run_script(script().filename());
            let exec_err = failed && fatal_error_recorded();
            ctx.tearing_down = true;
            ctx.errored = exec_err;
            rapira_request_shutdown();
            // RSHUTDOWN seals inside a zend_try; a bailout there leaves the seal to this call.
            ctx.end(exec_err);
            exec_err
        };
        unbind_server_context();
        errored
    }

    fn run_worker_handler(
        &self,
        ctx: &mut Context,
        fci: *mut zend_fcall_info,
        fcc: *mut zend_fcall_info_cache,
    ) -> (c_int, c_int, c_int) {
        bind_server_context(ctx);
        unsafe {
            populate_request_context(ctx);
            rapira_release_temporary_streams();
        }
        let mut outcome = unsafe { rapira_request_activate() };
        if outcome != RAPIRA_BAILOUT as c_int {
            unsafe {
                crate::context::apply_proto_num(ctx);
                outcome = rapira_run_handler(fci, fcc);
            }
        }
        ctx.tearing_down = true;
        let flushed = if outcome == RAPIRA_OK as c_int {
            RAPIRA_OK as c_int
        } else {
            unsafe { rapira_finish_output() }
        };
        ctx.errored = outcome != RAPIRA_OK as c_int || flushed == RAPIRA_BAILOUT as c_int;
        let teardown = unsafe { rapira_request_teardown() };
        (outcome, flushed, teardown)
    }

    fn exit_boot_failed(&self, code: i32) -> bool {
        crate::exit::exit(code)
    }

    fn pull_to_shed(
        &self,
        worker: &Worker<Self>,
        retry_at: Instant,
        retry: &Weak<BootRetry>,
    ) -> Pull {
        worker.units.pull_to_shed(retry_at, retry)
    }
}

/// Decides how an app boot reacts to OPcache state and the previous instance's `last_restart_time`.
/// An instance that boots while a restart is pending has OPcache off for its whole life, and the restart runs
/// only when no app holds the usage lock (`ZEND_RINIT_FUNCTION(zend_accelerator)` in `ZendAccelerator.c`).
fn opcache_boot(
    state: c_int,
    last_restart: zend_long,
    previous_end: Option<zend_long>,
) -> OpcacheBoot {
    if state < 0 {
        return OpcacheBoot::Run;
    }
    if state & OPCACHE_ON != 0 {
        return if state & OPCACHE_PENDING != 0 {
            OpcacheBoot::LeaveAtFirstPull
        } else {
            OpcacheBoot::Run
        };
    }
    let restart = state & (OPCACHE_PENDING | OPCACHE_RESTARTING) != 0
        || previous_end.is_some_and(|end| end != last_restart);
    if restart {
        OpcacheBoot::Wait {
            seen: previous_end.unwrap_or(last_restart),
        }
    } else {
        OpcacheBoot::Run
    }
}

fn give_up_boot(failures: u32, limit: u32, served: bool) -> bool {
    limit != 0 && failures >= limit && !served
}

/// `opcache_reset()` holds the shared allocator lock while scheduling a restart.
/// https://github.com/php/php-src/blob/php-8.6.0/ext/opcache/zend_accelerator_module.c#L935-L938
/// OPcache calls this hook before setting `restart_pending`.
/// https://github.com/php/php-src/blob/php-8.6.0/ext/opcache/ZendAccelerator.c#L3797-L3811
pub(crate) unsafe extern "C" fn opcache_restart_scheduled(reason: c_int) {
    let _ = with_active(|w| w.on_opcache_restart_scheduled(reason));
}

/// # Safety
/// Called from the C `handle_request` shell on a resident worker thread inside its active request with valid `fci` and `fcc`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_handle_request(
    fci: *mut zend_fcall_info,
    fcc: *mut zend_fcall_info_cache,
) -> c_int {
    let action = guard(RAPIRA_HANDLE_RECYCLE, || {
        with_active(|w| w.handle_request(fci, fcc)).unwrap_or(RAPIRA_HANDLE_STOP)
    });
    unbind_server_context();
    action as c_int
}

/// # Safety
/// Called from C (`rapira_finish_request`) on a worker thread inside an active request whose `Context` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_finish_response() {
    guard((), || unsafe {
        if let Some(c) = ctx() {
            // Shutdown functions, destructors, and teardown flush run after `ctx.errored` is set.
            let errored = c.errored || fatal_error_recorded();
            c.end(errored);
        }
    });
}

/// # Safety
/// `rv` is writable and the PHP engine is active on this thread.
unsafe fn receive_into_zval(rv: *mut zval, mode: RecvMode) -> bool {
    let outcome = with_active(|w| w.receive_dispatcher(mode)).unwrap_or(ReceiveOutcome::Closed);
    unsafe {
        match outcome {
            ReceiveOutcome::Unit(ptr) => {
                let ptr = NonNull::new_unchecked(ptr);
                // A Zend OOM bailout in `rapira_unit_object` longjmps past this frame, so this frame holds no Drop values; `current_unit` reclaims the box.
                rapira_unit_object(rv, ptr.as_ref().class())
                    .cast::<HeldPtr>()
                    .write(HeldPtr(Some(ptr)));
                true
            }
            ReceiveOutcome::Busy => {
                // SAFETY: a dispatcher unit has a static plugin record and a static error message.
                zend::throw_error(CStr::from_ptr((*rapira_current_plugin()).busy));
                false
            }
            ReceiveOutcome::Closed => {
                zend::throw_exception(rapira_ce_closed_exception, CLOSED_MSG);
                false
            }
            ReceiveOutcome::TimedOut => {
                zend::throw_exception(rapira_ce_timeout_exception, TIMEOUT_MSG);
                false
            }
            ReceiveOutcome::Empty => false,
        }
    }
}

/// # Safety
/// `timeout_us >= -1`. `rv` is the return value of the receive shell, and the engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, rv: *mut zval) -> bool {
    guard(false, || unsafe {
        receive_into_zval(rv, RecvMode::Wait(timeout_us))
    })
}

/// # Safety
/// As `rapira_rs_receive`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(rv: *mut zval) -> bool {
    guard(false, || unsafe { receive_into_zval(rv, RecvMode::Try) })
}

/// Writes the two counts of a `DispatcherInfo` object: intake depth and whether the last handed-out unit is unfinalized.
/// # Safety
/// `pending` and `active` are writable, and the engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_dispatcher_counts(pending: *mut i64, active: *mut i64) {
    guard((), || unsafe {
        let (p, a) = with_active(|w| (w.pool.pending() as i64, i64::from(w.has_active_unit())))
            .unwrap_or((0, 0));
        *pending = p;
        *active = a;
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crossbeam_channel::{Sender, bounded, never};

    use super::*;
    use crate::engine::Workers;
    use crate::work::{Entry, Idle, Unit, Units};

    #[derive(Default)]
    struct FakeHeldState {
        settled: AtomicBool,
        cancelled: AtomicBool,
        discarded: AtomicUsize,
        lost: AtomicUsize,
        shed: AtomicUsize,
        dropped: AtomicUsize,
    }

    struct FakeHeld(Arc<FakeHeldState>);

    impl Held for FakeHeld {
        fn settled(&self) -> bool {
            self.0.settled.load(Ordering::Relaxed)
        }
        fn cancelled(&self) -> bool {
            self.0.cancelled.load(Ordering::Relaxed)
        }
        fn discard(&mut self) {
            self.0.discarded.fetch_add(1, Ordering::Relaxed);
            self.0.settled.store(true, Ordering::Relaxed);
        }
        fn lost(&mut self) {
            self.0.lost.fetch_add(1, Ordering::Relaxed);
        }
        fn shed(self: Box<Self>) {
            self.0.shed.fetch_add(1, Ordering::Relaxed);
        }
        fn class(&self) -> *mut crate::zend_class_entry {
            null_mut()
        }
    }

    impl Drop for FakeHeld {
        fn drop(&mut self) {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dispatcher_unit(cancelled: bool) -> (Unit, Arc<FakeHeldState>) {
        let state = Arc::new(FakeHeldState {
            cancelled: AtomicBool::new(cancelled),
            ..FakeHeldState::default()
        });
        (
            Unit::Dispatcher(Box::new(FakeHeld(Arc::clone(&state)))),
            state,
        )
    }

    fn cgi_unit(cancelled: bool) -> (Unit, Arc<std::sync::Mutex<Vec<String>>>) {
        let (responder, log) = crate::types::tests::responder(cancelled);
        let mut ctx = crate::types::tests::context();
        ctx.responder = Some(responder);
        (Unit::Cgi(Box::new(ctx)), log)
    }

    struct ResetScript {
        started: bool,
        clean_shutdown: bool,
        hook_during_reset: Option<c_int>,
        hook_after_reset: Option<c_int>,
    }

    struct FakeState {
        prepare_calls: usize,
        reset_calls: usize,
        reset_scripts: VecDeque<ResetScript>,
        opcache_states: VecDeque<(c_int, zend_long)>,
        last_opcache_state: (c_int, zend_long),
        boot_starts: VecDeque<bool>,
        teardown_bailouts: VecDeque<bool>,
        teardown_calls: usize,
        stash_calls: usize,
        shutdown_bailouts: VecDeque<bool>,
        shutdown_calls: usize,
        disarm_calls: usize,
        arm_calls: usize,
        classic_calls: usize,
        classic_errors: VecDeque<bool>,
        handler_calls: usize,
        handler_outcomes: VecDeque<(c_int, c_int, c_int)>,
        exit_codes: Vec<i32>,
        shed_pulls: usize,
        script_cycles: Vec<u64>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                prepare_calls: 0,
                reset_calls: 0,
                reset_scripts: VecDeque::new(),
                opcache_states: VecDeque::new(),
                last_opcache_state: (-1, 0),
                boot_starts: VecDeque::new(),
                teardown_bailouts: VecDeque::new(),
                teardown_calls: 0,
                stash_calls: 0,
                shutdown_bailouts: VecDeque::new(),
                shutdown_calls: 0,
                disarm_calls: 0,
                arm_calls: 0,
                classic_calls: 0,
                classic_errors: VecDeque::new(),
                handler_calls: 0,
                handler_outcomes: VecDeque::new(),
                exit_codes: Vec::new(),
                shed_pulls: 0,
                script_cycles: Vec::new(),
            }
        }
    }

    type ScriptFn = Rc<dyn Fn(&Worker<FakePhp>)>;

    struct FakePhp {
        state: Rc<RefCell<FakeState>>,
        script: ScriptFn,
    }

    impl FakePhp {
        fn new(state: Rc<RefCell<FakeState>>, script: impl Fn(&Worker<FakePhp>) + 'static) -> Self {
            Self {
                state,
                script: Rc::new(script),
            }
        }
    }

    impl PhpRuntime for FakePhp {
        fn prepare_entrypoint(&self, _script: &Path) {
            self.state.borrow_mut().prepare_calls += 1;
        }

        fn run_opcache_reset_request(&self, worker: &Worker<Self>) -> (bool, bool) {
            let script = {
                let mut st = self.state.borrow_mut();
                st.reset_calls += 1;
                st.reset_scripts.pop_front().unwrap_or(ResetScript {
                    started: true,
                    clean_shutdown: true,
                    hook_during_reset: None,
                    hook_after_reset: None,
                })
            };
            if script.started {
                worker.with_host_reset(|| {
                    if let Some(reason) = script.hook_during_reset {
                        worker.on_opcache_restart_scheduled(reason);
                    }
                });
            }
            if let Some(reason) = script.hook_after_reset {
                worker.on_opcache_restart_scheduled(reason);
            }
            (script.started, script.clean_shutdown)
        }

        fn opcache_state(&self) -> (c_int, zend_long) {
            let mut st = self.state.borrow_mut();
            if let Some(next) = st.opcache_states.pop_front() {
                st.last_opcache_state = next;
                next
            } else {
                st.last_opcache_state
            }
        }

        fn boot_request_startup(&self) -> bool {
            self.state
                .borrow_mut()
                .boot_starts
                .pop_front()
                .unwrap_or(true)
        }

        fn execute_entrypoint(worker: &Worker<Self>) {
            worker
                .php
                .state
                .borrow_mut()
                .script_cycles
                .push(worker.cycle());
            let script = Rc::clone(&worker.php.script);
            script(worker);
        }

        fn teardown_request(&self) -> bool {
            let mut st = self.state.borrow_mut();
            st.teardown_calls += 1;
            st.teardown_bailouts.pop_front().unwrap_or(false)
        }

        fn stash_boot_shutdown_functions(&self) {
            self.state.borrow_mut().stash_calls += 1;
        }

        fn shutdown_request(&self) -> bool {
            let mut st = self.state.borrow_mut();
            st.shutdown_calls += 1;
            st.shutdown_bailouts.pop_front().unwrap_or(false)
        }

        fn log_and_clear_last_error(&self) {}

        fn disarm_timer(&self) {
            self.state.borrow_mut().disarm_calls += 1;
        }

        fn arm_timer(&self) {
            self.state.borrow_mut().arm_calls += 1;
        }

        fn run_classic_unit(&self, ctx: &mut Context) -> bool {
            let errored = {
                let mut st = self.state.borrow_mut();
                st.classic_calls += 1;
                st.classic_errors.pop_front().unwrap_or(false)
            };
            ctx.end(errored);
            errored
        }

        fn run_worker_handler(
            &self,
            ctx: &mut Context,
            _fci: *mut zend_fcall_info,
            _fcc: *mut zend_fcall_info_cache,
        ) -> (c_int, c_int, c_int) {
            let out = {
                let mut st = self.state.borrow_mut();
                st.handler_calls += 1;
                st.handler_outcomes.pop_front().unwrap_or((
                    RAPIRA_OK as c_int,
                    RAPIRA_OK as c_int,
                    RAPIRA_OK as c_int,
                ))
            };
            let errored = out.0 != RAPIRA_OK as c_int || out.1 == RAPIRA_BAILOUT as c_int;
            ctx.end(errored);
            out
        }

        fn exit_boot_failed(&self, code: i32) -> bool {
            self.state.borrow_mut().exit_codes.push(code);
            true
        }

        fn pull_to_shed(
            &self,
            worker: &Worker<Self>,
            _retry_at: Instant,
            _retry: &Weak<BootRetry>,
        ) -> Pull {
            self.state.borrow_mut().shed_pulls += 1;
            let disabled = never();
            let rx = if worker.pool.ready() {
                &disabled
            } else {
                &worker.units.rx
            };
            worker.units.recv(rx, Duration::ZERO)
        }
    }

    struct Harness {
        worker: Worker<FakePhp>,
        tx: Sender<Entry>,
        control: Sender<()>,
        pool: Arc<Pool>,
        opcache: Arc<OpcacheCoordinator>,
        retry: Arc<BootRetry>,
        state: Rc<RefCell<FakeState>>,
    }

    impl Harness {
        fn new(
            max_requests: u64,
            state: FakeState,
            script: impl Fn(&Worker<FakePhp>) + 'static,
        ) -> Self {
            let pool = Arc::new(Pool::new("test", 1));
            let (tx, rx) = bounded(16);
            let units = Units {
                rx,
                idle: Arc::<Idle>::default(),
                pool: Arc::clone(&pool),
            };
            let workers = Arc::<Workers>::default();
            let (unit_rx, control) = UnitRx::new(units, workers);
            let opcache = Arc::<OpcacheCoordinator>::default();
            let state = Rc::new(RefCell::new(state));
            let worker = Worker::new(
                unit_rx,
                Arc::clone(&pool),
                0,
                Arc::clone(&opcache),
                max_requests,
                FakePhp::new(Rc::clone(&state), script),
            );
            Self {
                worker,
                tx,
                control,
                pool,
                opcache,
                retry: Arc::default(),
                state,
            }
        }

        fn push(&self, unit: Unit) {
            self.tx
                .send(Entry {
                    unit,
                    budget_bytes: 0,
                })
                .unwrap();
        }
    }

    #[test]
    fn a_failed_boot_exits_only_before_the_pool_serves() {
        struct Case {
            name: &'static str,
            failures: u32,
            limit: u32,
            served: bool,
            exit: bool,
        }
        let cases = [
            Case {
                name: "the next boot can still succeed",
                failures: 2,
                limit: 3,
                served: false,
                exit: false,
            },
            Case {
                name: "the pool never served and this worker reached the limit",
                failures: 3,
                limit: 3,
                served: false,
                exit: true,
            },
            Case {
                name: "another worker served before this worker reached the limit",
                failures: 3,
                limit: 3,
                served: true,
                exit: false,
            },
            Case {
                name: "a pool that served continues to retry",
                failures: 4,
                limit: 3,
                served: true,
                exit: false,
            },
            Case {
                name: "zero disables the limit even if the pool never served",
                failures: 5,
                limit: 0,
                served: false,
                exit: false,
            },
        ];
        for case in cases {
            assert_eq!(
                give_up_boot(case.failures, case.limit, case.served),
                case.exit,
                "{}",
                case.name
            );
        }
    }

    fn text(boot: &OpcacheBoot) -> String {
        match boot {
            OpcacheBoot::Run => "run".to_owned(),
            OpcacheBoot::LeaveAtFirstPull => "leave at the first pull".to_owned(),
            OpcacheBoot::Wait { seen } => format!("wait until last_restart_time is not {seen}"),
        }
    }

    #[test]
    fn opcache_boot_follows_the_opcache_state() {
        struct Case {
            name: &'static str,
            state: c_int,
            last_restart: zend_long,
            previous_end: Option<zend_long>,
            want: OpcacheBoot,
        }
        let cases = [
            Case {
                name: "no OPcache",
                state: -1,
                last_restart: 0,
                previous_end: None,
                want: OpcacheBoot::Run,
            },
            Case {
                name: "OPcache on",
                state: 1,
                last_restart: 7,
                previous_end: Some(7),
                want: OpcacheBoot::Run,
            },
            Case {
                name: "on, and a restart is pending",
                state: 1 | 2,
                last_restart: 7,
                previous_end: Some(7),
                want: OpcacheBoot::LeaveAtFirstPull,
            },
            Case {
                name: "off, and a restart is pending",
                state: 2,
                last_restart: 7,
                previous_end: Some(7),
                want: OpcacheBoot::Wait { seen: 7 },
            },
            Case {
                name: "off, a restart is pending, first app instance of a process",
                state: 2,
                last_restart: 7,
                previous_end: None,
                want: OpcacheBoot::Wait { seen: 7 },
            },
            Case {
                name: "off, and a restart runs now",
                state: 4,
                last_restart: 7,
                previous_end: Some(7),
                want: OpcacheBoot::Wait { seen: 7 },
            },
            Case {
                name: "off, and a restart ran after the previous app instance",
                state: 0,
                last_restart: 8,
                previous_end: Some(7),
                want: OpcacheBoot::Wait { seen: 7 },
            },
            Case {
                name: "off, and no restart ran after the previous app instance",
                state: 0,
                last_restart: 7,
                previous_end: Some(7),
                want: OpcacheBoot::Run,
            },
            Case {
                name: "off with no sign of a restart, first app instance of a process",
                state: 0,
                last_restart: 7,
                previous_end: None,
                want: OpcacheBoot::Run,
            },
        ];
        for case in cases {
            let got = opcache_boot(case.state, case.last_restart, case.previous_end);
            assert_eq!(text(&got), text(&case.want), "{}", case.name);
        }
    }

    #[test]
    fn worker_mode_handle_request_enforces_quota_teardown_and_outcomes() {
        struct Case {
            name: &'static str,
            max_requests: u64,
            units: usize,
            teardown_bailouts: &'static [bool],
            handler_outcomes: &'static [(c_int, c_int, c_int)],
            want_actions: &'static [u32],
            want_handled: u64,
            want_errors: u64,
            want_recycles: u64,
            want_stashes: usize,
        }
        const OK: (c_int, c_int, c_int) =
            (RAPIRA_OK as c_int, RAPIRA_OK as c_int, RAPIRA_OK as c_int);
        let cases = [
            Case {
                name: "quota recycles after max_requests and starts a second cycle",
                max_requests: 2,
                units: 3,
                teardown_bailouts: &[false, false],
                handler_outcomes: &[OK, OK, OK],
                want_actions: &[
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_STOP,
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_STOP,
                ],
                want_handled: 3,
                want_errors: 0,
                want_recycles: 1,
                want_stashes: 2,
            },
            Case {
                name: "first call teardown bailout recycles without stashing or pulling",
                max_requests: 0,
                units: 1,
                teardown_bailouts: &[true, false],
                handler_outcomes: &[OK],
                want_actions: &[
                    RAPIRA_HANDLE_RECYCLE,
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_STOP,
                ],
                want_handled: 1,
                want_errors: 0,
                want_recycles: 1,
                want_stashes: 1,
            },
            Case {
                name: "handler bailout marks errored and recycles before next unit",
                max_requests: 0,
                units: 2,
                teardown_bailouts: &[false, false],
                handler_outcomes: &[
                    (
                        RAPIRA_BAILOUT as c_int,
                        RAPIRA_OK as c_int,
                        RAPIRA_OK as c_int,
                    ),
                    OK,
                ],
                want_actions: &[
                    RAPIRA_HANDLE_RECYCLE,
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_STOP,
                ],
                want_handled: 2,
                want_errors: 1,
                want_recycles: 1,
                want_stashes: 2,
            },
            Case {
                name: "handler throw marks errored and continues in same cycle",
                max_requests: 0,
                units: 2,
                teardown_bailouts: &[false],
                handler_outcomes: &[
                    (
                        RAPIRA_THROW as c_int,
                        RAPIRA_OK as c_int,
                        RAPIRA_OK as c_int,
                    ),
                    OK,
                ],
                want_actions: &[
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_STOP,
                ],
                want_handled: 2,
                want_errors: 1,
                want_recycles: 0,
                want_stashes: 1,
            },
            Case {
                name: "teardown bailout after ok handler marks errored and recycles",
                max_requests: 0,
                units: 2,
                teardown_bailouts: &[false, false],
                handler_outcomes: &[
                    (
                        RAPIRA_OK as c_int,
                        RAPIRA_OK as c_int,
                        RAPIRA_BAILOUT as c_int,
                    ),
                    OK,
                ],
                want_actions: &[
                    RAPIRA_HANDLE_RECYCLE,
                    RAPIRA_HANDLE_CONTINUE,
                    RAPIRA_HANDLE_STOP,
                ],
                want_handled: 2,
                want_errors: 1,
                want_recycles: 1,
                want_stashes: 2,
            },
        ];

        for case in cases {
            let actions = Rc::new(RefCell::new(Vec::new()));
            let actions_ref = Rc::clone(&actions);
            let fake = FakeState {
                teardown_bailouts: case.teardown_bailouts.iter().copied().collect(),
                handler_outcomes: case.handler_outcomes.iter().copied().collect(),
                ..FakeState::default()
            };
            let h = Harness::new(case.max_requests, fake, move |w| {
                loop {
                    let action = w.handle_request(null_mut(), null_mut());
                    actions_ref.borrow_mut().push(action);
                    if action != RAPIRA_HANDLE_CONTINUE {
                        break;
                    }
                }
            });
            for _ in 0..case.units {
                let (unit, _log) = cgi_unit(false);
                h.push(unit);
            }
            drop(h.tx);
            let retry_weak = Arc::downgrade(&h.retry);
            h.worker
                .run_resident(Path::new("worker.php"), 3, &retry_weak);

            let snap = h.pool.snapshot();
            let ws = &snap.workers[0];
            assert_eq!(
                actions.borrow().as_slice(),
                case.want_actions,
                "{}",
                case.name
            );
            assert_eq!(ws.handled, case.want_handled, "{}", case.name);
            assert_eq!(ws.errors, case.want_errors, "{}", case.name);
            assert_eq!(ws.recycles, case.want_recycles, "{}", case.name);
            assert_eq!(
                h.state.borrow().stash_calls,
                case.want_stashes,
                "{}",
                case.name
            );
            let expected_cycles: Vec<u64> = (1..=case.want_recycles + 1).collect();
            assert_eq!(
                h.state.borrow().script_cycles,
                expected_cycles,
                "{}",
                case.name
            );
            drop(h.control);
        }
    }

    #[test]
    fn dispatcher_mode_receive_tracks_quota_busy_cancelled_and_reclaim() {
        enum Scenario {
            SettledCountsOnceAndRecycles,
            ForgetHeldCountsOnceAndIgnoresStale,
            TryReceiveEmptyDoesNotDoubleCount,
            UnsettledLiveReturnsBusy,
            UnsettledCancelledDiscardsAndProceeds,
            LeakedUnitReclaimedAtCycleEnd,
        }
        struct Case {
            name: &'static str,
            scenario: Scenario,
        }
        let cases = [
            Case {
                name: "settled unit counts quota once on next receive and recycles at max_requests",
                scenario: Scenario::SettledCountsOnceAndRecycles,
            },
            Case {
                name: "forget_held counts quota once and stale pointer does not disturb next unit",
                scenario: Scenario::ForgetHeldCountsOnceAndIgnoresStale,
            },
            Case {
                name: "settled unit followed by empty try_receive does not double count on forget_held",
                scenario: Scenario::TryReceiveEmptyDoesNotDoubleCount,
            },
            Case {
                name: "unsettled live unit returns Busy without counting quota",
                scenario: Scenario::UnsettledLiveReturnsBusy,
            },
            Case {
                name: "unsettled cancelled unit is discarded before pulling next unit",
                scenario: Scenario::UnsettledCancelledDiscardsAndProceeds,
            },
            Case {
                name: "unreclaimed unit at cycle end is reclaimed and recorded as errored",
                scenario: Scenario::LeakedUnitReclaimedAtCycleEnd,
            },
        ];

        for case in cases {
            match case.scenario {
                Scenario::SettledCountsOnceAndRecycles => {
                    let h = Harness::new(1, FakeState::default(), |_| {});
                    let (u1, s1) = dispatcher_unit(false);
                    let (u2, s2) = dispatcher_unit(false);
                    h.push(u1);
                    h.push(u2);

                    let ReceiveOutcome::Unit(p1) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected first unit", case.name);
                    };
                    assert!(h.worker.has_active_unit(), "{}", case.name);
                    assert_eq!(h.state.borrow().arm_calls, 1, "{}", case.name);

                    s1.settled.store(true, Ordering::Relaxed);
                    assert!(!h.worker.has_active_unit(), "{}", case.name);
                    assert!(
                        matches!(
                            h.worker.receive_dispatcher(RecvMode::Wait(-1)),
                            ReceiveOutcome::Closed
                        ),
                        "{}: quota=1 should close on next receive",
                        case.name
                    );
                    assert_eq!(h.worker.outcome(), Some(End::Recycle), "{}", case.name);
                    assert_eq!(h.state.borrow().disarm_calls, 1, "{}", case.name);

                    // Clean up leaked p1 and verify u2 stayed queued.
                    h.worker.forget_held(p1.cast::<()>());
                    drop(unsafe { Box::from_raw(p1) });
                    assert_eq!(s1.dropped.load(Ordering::Relaxed), 1, "{}", case.name);
                    assert_eq!(s2.dropped.load(Ordering::Relaxed), 0, "{}", case.name);
                }
                Scenario::ForgetHeldCountsOnceAndIgnoresStale => {
                    let h = Harness::new(2, FakeState::default(), |_| {});
                    let (u1, s1) = dispatcher_unit(false);
                    let (u2, s2) = dispatcher_unit(false);
                    let (u3, _s3) = dispatcher_unit(false);
                    h.push(u1);
                    h.push(u2);
                    h.push(u3);

                    let ReceiveOutcome::Unit(p1) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected u1", case.name);
                    };
                    s1.settled.store(true, Ordering::Relaxed);
                    // Drop u1 via forget_held before calling receive().
                    h.worker.forget_held(p1.cast::<()>());
                    drop(unsafe { Box::from_raw(p1) });
                    assert_eq!(h.worker.served.get(), 1, "{}", case.name);

                    // Pull u2.
                    let ReceiveOutcome::Unit(p2) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected u2", case.name);
                    };
                    // Stale forget_held(p1) must not clear p2 or tick quota.
                    h.worker.forget_held(p1.cast::<()>());
                    assert!(h.worker.has_active_unit(), "{}", case.name);
                    assert_eq!(h.worker.served.get(), 1, "{}", case.name);

                    s2.settled.store(true, Ordering::Relaxed);
                    assert!(
                        matches!(
                            h.worker.receive_dispatcher(RecvMode::Wait(-1)),
                            ReceiveOutcome::Closed
                        ),
                        "{}: second unit hits quota=2",
                        case.name
                    );
                    assert_eq!(h.worker.served.get(), 2, "{}", case.name);
                    h.worker.forget_held(p2.cast::<()>());
                    drop(unsafe { Box::from_raw(p2) });
                }
                Scenario::TryReceiveEmptyDoesNotDoubleCount => {
                    let h = Harness::new(2, FakeState::default(), |_| {});
                    let (u1, s1) = dispatcher_unit(false);
                    h.push(u1);

                    let ReceiveOutcome::Unit(p1) = h.worker.receive_dispatcher(RecvMode::Try)
                    else {
                        panic!("{}: expected u1", case.name);
                    };
                    s1.settled.store(true, Ordering::Relaxed);
                    assert!(
                        matches!(
                            h.worker.receive_dispatcher(RecvMode::Try),
                            ReceiveOutcome::Empty
                        ),
                        "{}",
                        case.name
                    );
                    assert_eq!(h.worker.served.get(), 1, "{}", case.name);
                    h.worker.forget_held(p1.cast::<()>());
                    drop(unsafe { Box::from_raw(p1) });
                    assert_eq!(h.worker.served.get(), 1, "{}", case.name);
                    assert!(!h.worker.ended(), "{}", case.name);
                }
                Scenario::UnsettledLiveReturnsBusy => {
                    let h = Harness::new(2, FakeState::default(), |_| {});
                    let (u1, s1) = dispatcher_unit(false);
                    let (u2, _s2) = dispatcher_unit(false);
                    h.push(u1);
                    h.push(u2);

                    let ReceiveOutcome::Unit(p1) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected u1", case.name);
                    };
                    assert!(
                        matches!(
                            h.worker.receive_dispatcher(RecvMode::Wait(-1)),
                            ReceiveOutcome::Busy
                        ),
                        "{}",
                        case.name
                    );
                    assert_eq!(h.worker.served.get(), 0, "{}", case.name);
                    s1.settled.store(true, Ordering::Relaxed);
                    let ReceiveOutcome::Unit(p2) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected u2 after settling u1", case.name);
                    };
                    assert_eq!(h.worker.served.get(), 1, "{}", case.name);
                    drop(unsafe { Box::from_raw(p1) });
                    h.worker.forget_held(p2.cast::<()>());
                    drop(unsafe { Box::from_raw(p2) });
                }
                Scenario::UnsettledCancelledDiscardsAndProceeds => {
                    let h = Harness::new(0, FakeState::default(), |_| {});
                    let (u1, s1) = dispatcher_unit(false);
                    let (u2, _s2) = dispatcher_unit(false);
                    h.push(u1);
                    h.push(u2);

                    let ReceiveOutcome::Unit(p1) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected u1", case.name);
                    };
                    s1.cancelled.store(true, Ordering::Relaxed);
                    let ReceiveOutcome::Unit(p2) = h.worker.receive_dispatcher(RecvMode::Wait(-1))
                    else {
                        panic!("{}: expected u2 after discarding cancelled u1", case.name);
                    };
                    assert_eq!(s1.discarded.load(Ordering::Relaxed), 1, "{}", case.name);
                    assert_eq!(h.worker.served.get(), 1, "{}", case.name);
                    drop(unsafe { Box::from_raw(p1) });
                    h.worker.forget_held(p2.cast::<()>());
                    drop(unsafe { Box::from_raw(p2) });
                }
                Scenario::LeakedUnitReclaimedAtCycleEnd => {
                    let (u1, s1) = dispatcher_unit(false);
                    let h = Harness::new(0, FakeState::default(), |w| {
                        let ReceiveOutcome::Unit(_ptr) = w.receive_dispatcher(RecvMode::Wait(-1))
                        else {
                            panic!("expected leaked unit");
                        };
                        // Return from script without freeing the unit (simulates OOM/bailout).
                    });
                    h.push(u1);
                    let end = h.worker.run_cycle(Path::new("dispatcher.php"));
                    assert_eq!(end, Some(End::Recycle), "{}", case.name);
                    assert_eq!(s1.dropped.load(Ordering::Relaxed), 1, "{}", case.name);
                    assert!(!h.worker.has_active_unit(), "{}", case.name);
                    let snap = h.pool.snapshot();
                    assert_eq!(snap.workers[0].handled, 1, "{}", case.name);
                    assert_eq!(snap.workers[0].errors, 1, "{}", case.name);
                }
            }
        }
    }

    #[test]
    fn resident_loop_handles_boot_failures_shedding_and_exit_70() {
        enum ModeCase {
            HitLimitExits70,
            ShedThenRecover,
            AlreadyServedDoesNotExit70,
            DrainingSuppressesExit70,
        }
        struct Case {
            name: &'static str,
            mode: ModeCase,
        }
        let cases = [
            Case {
                name: "consecutive boot failures hit limit before pool serves and exit 70",
                mode: ModeCase::HitLimitExits70,
            },
            Case {
                name: "boot failures shed queued units during backoff and recover on next boot",
                mode: ModeCase::ShedThenRecover,
            },
            Case {
                name: "boot failure after pool already served does not exit 70",
                mode: ModeCase::AlreadyServedDoesNotExit70,
            },
            Case {
                name: "draining pool suppresses exit 70 on boot failure",
                mode: ModeCase::DrainingSuppressesExit70,
            },
        ];

        for case in cases {
            match case.mode {
                ModeCase::HitLimitExits70 => {
                    let fake = FakeState {
                        boot_starts: VecDeque::from([false, false, false]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |_| {});
                    let retry_weak = Arc::downgrade(&h.retry);
                    h.worker.run_resident(Path::new("bad.php"), 3, &retry_weak);
                    assert_eq!(h.state.borrow().exit_codes, vec![70], "{}", case.name);
                    assert_eq!(h.pool.snapshot().workers[0].recycles, 0, "{}", case.name);
                    drop(h.tx);
                    drop(h.control);
                }
                ModeCase::ShedThenRecover => {
                    let fake = FakeState {
                        boot_starts: VecDeque::from([false, true]),
                        ..FakeState::default()
                    };
                    let tx_slot: Rc<RefCell<Option<Sender<Entry>>>> = Rc::new(RefCell::new(None));
                    let tx_for_script = Rc::clone(&tx_slot);
                    let h = Harness::new(0, fake, move |w| {
                        if let Some(tx) = tx_for_script.borrow_mut().take() {
                            let (u, _log) = cgi_unit(false);
                            tx.send(Entry {
                                unit: u,
                                budget_bytes: 0,
                            })
                            .unwrap();
                            drop(tx);
                        }
                        while w.handle_request(null_mut(), null_mut()) == RAPIRA_HANDLE_CONTINUE {}
                    });
                    let (u_shed, s_shed) = dispatcher_unit(false);
                    h.push(u_shed);
                    *tx_slot.borrow_mut() = Some(h.tx);
                    let retry_weak = Arc::downgrade(&h.retry);
                    h.worker
                        .run_resident(Path::new("recover.php"), 3, &retry_weak);

                    assert!(h.state.borrow().exit_codes.is_empty(), "{}", case.name);
                    assert_eq!(s_shed.shed.load(Ordering::Relaxed), 1, "{}", case.name);
                    let ws = &h.pool.snapshot().workers[0];
                    assert_eq!(ws.handled, 2, "{}", case.name);
                    assert_eq!(ws.errors, 1, "{}", case.name);
                    drop(h.control);
                }
                ModeCase::AlreadyServedDoesNotExit70 => {
                    let fake = FakeState {
                        boot_starts: VecDeque::from([false, false]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |_| {});
                    // Mark pool as having served previously.
                    h.pool.record_worker(0, Event::Idle);
                    h.pool.record_worker(0, Event::Starting);
                    assert!(h.pool.served(), "{}", case.name);
                    drop(h.tx);
                    let retry_weak = Arc::downgrade(&h.retry);
                    h.worker.run_resident(Path::new("bad.php"), 1, &retry_weak);
                    assert!(h.state.borrow().exit_codes.is_empty(), "{}", case.name);
                    drop(h.control);
                }
                ModeCase::DrainingSuppressesExit70 => {
                    let fake = FakeState {
                        boot_starts: VecDeque::from([false]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |_| {});
                    h.pool.begin_drain();
                    drop(h.tx);
                    let retry_weak = Arc::downgrade(&h.retry);
                    h.worker.run_resident(Path::new("bad.php"), 1, &retry_weak);
                    assert!(h.state.borrow().exit_codes.is_empty(), "{}", case.name);
                    drop(h.control);
                }
            }
        }
    }

    #[test]
    fn opcache_lifecycle_handles_boot_wait_leave_reset_and_scheduled_restart() {
        enum Scenario {
            LeaveAtFirstPull,
            WaitUntilRestartTimeChanges,
            HostResetSuppressesHookWhileScriptHookWakes,
            FailedHostResetFailsCycleAndStaysPending,
        }
        struct Case {
            name: &'static str,
            scenario: Scenario,
        }
        let cases = [
            Case {
                name: "LeaveAtFirstPull recycles without taking queued unit and next cycle serves it",
                scenario: Scenario::LeaveAtFirstPull,
            },
            Case {
                name: "Wait recycles once last_restart_time changes",
                scenario: Scenario::WaitUntilRestartTimeChanges,
            },
            Case {
                name: "host reset ignores hook inside with_host_reset while script hook sets scheduled restart",
                scenario: Scenario::HostResetSuppressesHookWhileScriptHookWakes,
            },
            Case {
                name: "failed host opcache reset fails cycle without booting and keeps reset pending",
                scenario: Scenario::FailedHostResetFailsCycleAndStaysPending,
            },
        ];

        for case in cases {
            match case.scenario {
                Scenario::LeaveAtFirstPull => {
                    // Cycle 1 boot: ON|PENDING, Cycle 1 end: ON|PENDING, Cycle 2 boot: ON, Cycle 2 end: ON.
                    let fake = FakeState {
                        opcache_states: VecDeque::from([
                            (OPCACHE_ON | OPCACHE_PENDING, 10),
                            (OPCACHE_ON | OPCACHE_PENDING, 10),
                            (OPCACHE_ON, 11),
                            (OPCACHE_ON, 11),
                        ]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |w| {
                        while w.handle_request(null_mut(), null_mut()) == RAPIRA_HANDLE_CONTINUE {}
                    });
                    let (u1, _log) = cgi_unit(false);
                    h.push(u1);
                    drop(h.tx);
                    let retry_weak = Arc::downgrade(&h.retry);
                    h.worker.run_resident(Path::new("app.php"), 3, &retry_weak);
                    assert_eq!(h.state.borrow().script_cycles, vec![1, 2], "{}", case.name);
                    let ws = &h.pool.snapshot().workers[0];
                    assert_eq!(ws.handled, 1, "{}", case.name);
                    assert_eq!(ws.recycles, 1, "{}", case.name);
                    drop(h.control);
                }
                Scenario::WaitUntilRestartTimeChanges => {
                    // Cycle 1 boot: PENDING (off, last_restart=7) -> opcache_wait=Some(7).
                    // First next_pull checks opcache_restart_ran(): returns (ON, 8) -> changes -> recycles.
                    // Cycle 1 end: (ON, 8), Cycle 2 boot: (ON, 8), Cycle 2 end: (ON, 8).
                    let fake = FakeState {
                        opcache_states: VecDeque::from([
                            (OPCACHE_PENDING, 7),
                            (OPCACHE_ON, 8),
                            (OPCACHE_ON, 8),
                            (OPCACHE_ON, 8),
                            (OPCACHE_ON, 8),
                        ]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |w| {
                        while w.handle_request(null_mut(), null_mut()) == RAPIRA_HANDLE_CONTINUE {}
                    });
                    let (u1, _log) = cgi_unit(false);
                    h.push(u1);
                    drop(h.tx);
                    let retry_weak = Arc::downgrade(&h.retry);
                    h.worker.run_resident(Path::new("app.php"), 3, &retry_weak);
                    assert_eq!(h.state.borrow().script_cycles, vec![1, 2], "{}", case.name);
                    let ws = &h.pool.snapshot().workers[0];
                    assert_eq!(ws.handled, 1, "{}", case.name);
                    assert_eq!(ws.recycles, 1, "{}", case.name);
                    drop(h.control);
                }
                Scenario::HostResetSuppressesHookWhileScriptHookWakes => {
                    let fake = FakeState {
                        reset_scripts: VecDeque::from([ResetScript {
                            started: true,
                            clean_shutdown: true,
                            hook_during_reset: Some(2),
                            hook_after_reset: None,
                        }]),
                        ..FakeState::default()
                    };
                    let saw_scheduled_before_script = Rc::new(Cell::new(true));
                    let saw_ref = Rc::clone(&saw_scheduled_before_script);
                    let h = Harness::new(0, fake, move |w| {
                        saw_ref.set(w.opcache_scheduled.get().is_some());
                        // Simulate PHP script triggering OOM OPcache restart.
                        w.on_opcache_restart_scheduled(0);
                        assert_eq!(w.opcache_scheduled.get(), Some(0));
                    });
                    h.opcache.request_reset();
                    let end = h.worker.run_cycle(Path::new("app.php"));
                    assert_eq!(end, None, "{}", case.name);
                    assert!(!saw_scheduled_before_script.get(), "{}", case.name);
                    // Cycle shutdown flushed opcache_scheduled via reload_for_opcache_restart().
                    assert_eq!(h.worker.opcache_scheduled.get(), None, "{}", case.name);
                    assert_eq!(h.opcache.claim_reset(), None, "{}", case.name);
                    drop(h.tx);
                    drop(h.control);
                }
                Scenario::FailedHostResetFailsCycleAndStaysPending => {
                    let fake = FakeState {
                        reset_scripts: VecDeque::from([ResetScript {
                            started: false,
                            clean_shutdown: true,
                            hook_during_reset: None,
                            hook_after_reset: None,
                        }]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |_| {
                        panic!("entrypoint must not execute after failed opcache reset");
                    });
                    h.opcache.request_reset();
                    let end = h.worker.run_cycle(Path::new("app.php"));
                    assert_eq!(end, None, "{}", case.name);
                    assert_eq!(h.worker.cycle(), 0, "{}", case.name);
                    assert!(h.opcache.claim_reset().is_some(), "{}", case.name);
                    h.opcache.finish_reset(1, true);
                    drop(h.tx);
                    drop(h.control);
                }
            }
        }
    }

    #[test]
    fn classic_mode_handles_units_opcache_reset_failure_and_reload() {
        enum Scenario {
            ServesUnitsAndSkipsCancelled,
            FailedOpcacheResetRefuses500,
            ReloadReResolvesEntrypoint,
        }
        struct Case {
            name: &'static str,
            scenario: Scenario,
        }
        let cases = [
            Case {
                name: "serves CGI units, counts cancelled unit as shed, and stops on Closed",
                scenario: Scenario::ServesUnitsAndSkipsCancelled,
            },
            Case {
                name: "failed OPcache reset refuses CGI unit with 500 without running executor",
                scenario: Scenario::FailedOpcacheResetRefuses500,
            },
            Case {
                name: "Reload re-resolves entrypoint and serves subsequent unit",
                scenario: Scenario::ReloadReResolvesEntrypoint,
            },
        ];

        for case in cases {
            match case.scenario {
                Scenario::ServesUnitsAndSkipsCancelled => {
                    let h = Harness::new(0, FakeState::default(), |_| {});
                    let (u_cancelled, _log1) = cgi_unit(true);
                    let (u_ok, _log2) = cgi_unit(false);
                    h.push(u_cancelled);
                    h.push(u_ok);
                    drop(h.tx);
                    h.worker.run_classic(Path::new("index.php"));
                    let ws = &h.pool.snapshot().workers[0];
                    assert_eq!(ws.handled, 2, "{}", case.name);
                    assert_eq!(ws.errors, 1, "{}", case.name);
                    assert_eq!(h.state.borrow().classic_calls, 1, "{}", case.name);
                    assert_eq!(h.state.borrow().prepare_calls, 1, "{}", case.name);
                    drop(h.control);
                }
                Scenario::FailedOpcacheResetRefuses500 => {
                    let fake = FakeState {
                        reset_scripts: VecDeque::from([ResetScript {
                            started: false,
                            clean_shutdown: true,
                            hook_during_reset: None,
                            hook_after_reset: None,
                        }]),
                        ..FakeState::default()
                    };
                    let h = Harness::new(0, fake, |_| {});
                    h.opcache.request_reset();
                    let (u1, log1) = cgi_unit(false);
                    h.push(u1);
                    drop(h.tx);
                    h.worker.run_classic(Path::new("index.php"));
                    assert_eq!(
                        log1.lock().unwrap().as_slice(),
                        ["respond 500 truncated=false body="],
                        "{}",
                        case.name
                    );
                    assert_eq!(h.state.borrow().classic_calls, 0, "{}", case.name);
                    let ws = &h.pool.snapshot().workers[0];
                    assert_eq!(ws.handled, 1, "{}", case.name);
                    assert_eq!(ws.errors, 1, "{}", case.name);
                    drop(h.control);
                }
                Scenario::ReloadReResolvesEntrypoint => {
                    let h = Harness::new(0, FakeState::default(), |_| {});
                    let (u1, _log1) = cgi_unit(false);
                    h.control.send(()).unwrap();
                    h.push(u1);
                    drop(h.tx);
                    h.worker.run_classic(Path::new("index.php"));
                    assert_eq!(h.state.borrow().prepare_calls, 2, "{}", case.name);
                    assert_eq!(h.state.borrow().classic_calls, 1, "{}", case.name);
                    let ws = &h.pool.snapshot().workers[0];
                    assert_eq!(ws.handled, 1, "{}", case.name);
                    assert_eq!(ws.errors, 0, "{}", case.name);
                    drop(h.control);
                }
            }
        }
    }
}
