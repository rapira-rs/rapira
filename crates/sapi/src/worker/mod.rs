//! Per-worker runtime owning the Cycle, Pull, quota, OPcache reload state, and active Dispatcher Unit.

use std::cell::Cell;
use std::ffi::{CStr, c_int};
use std::path::Path;
use std::ptr::{NonNull, null, null_mut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use crate::callbacks::{fatal_error_recorded, guard};
use crate::context::{
    bind_server_context, clean_realpath_cache, ctx, populate_request_context, resolve_script,
    script, unbind_server_context,
};
use crate::diagnostics::{Severity, error_type_to_level};
use crate::engine::Workers;
use crate::pool::{Event, Pool};
use crate::queue::{Pull, Wait, WorkerSlot};
use crate::reload::OpcacheCoordinator;
use crate::start::run_script;
use crate::types::CgiExchange;
use crate::work::{Held, HeldPtr};
use crate::{
    E_CORE, FAILURE, RAPIRA_BAILOUT, RAPIRA_HANDLE_CONTINUE, RAPIRA_HANDLE_RECYCLE,
    RAPIRA_HANDLE_STOP, RAPIRA_OK, RAPIRA_THROW, SUCCESS, php_request_startup,
    rapira_ce_closed_exception, rapira_ce_timeout_exception, rapira_clear_last_error,
    rapira_current_plugin, rapira_eg, rapira_opcache_reset, rapira_opcache_state, rapira_pg,
    rapira_receive_timed, rapira_receive_untimed, rapira_release_temporary_streams,
    rapira_request_activate, rapira_request_shutdown, rapira_request_teardown, rapira_run_handler,
    rapira_sg, rapira_stash_boot_shutdown_functions, rapira_unit_object, zend, zend_fcall_info,
    zend_fcall_info_cache, zend_is_auto_global_str, zend_long, zval,
};

const CLOSED_MSG: &CStr = c"no more work for this app instance";
const TIMEOUT_MSG: &CStr = c"no work became available within the timeout";

/// Flags returned by `rapira_opcache_state()`.
const OPCACHE_ON: c_int = 1;
const OPCACHE_PENDING: c_int = 2;
const OPCACHE_RESTARTING: c_int = 4;

/// Interval at which an idle pull checks for a completed OPcache restart when OPcache booted off.
const OPCACHE_POLL: Duration = Duration::from_secs(1);

/// Wait after the first failed boot of a sequence; doubles per consecutive failure up to `BOOT_RETRY_MAX`.
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
    /// OPcache is off for this app instance. It leaves when `last_restart_time` is not `seen`.
    Wait {
        seen: zend_long,
    },
}

enum RecvMode {
    Wait(i64),
    Try,
}

thread_local! {
    /// Scoped pointer to the stack-owned `Worker` on this PHP thread, active during worker execution.
    static ACTIVE_WORKER: Cell<*const Worker> = const { Cell::new(null()) };
}

struct ActiveWorkerGuard(*const Worker);

impl ActiveWorkerGuard {
    fn enter(worker: &Worker) -> Self {
        Self(ACTIVE_WORKER.replace(std::ptr::from_ref(worker)))
    }
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        ACTIVE_WORKER.set(self.0);
    }
}

/// Runs `f` against the active `Worker` on this thread, if one is bound.
#[inline]
fn with_active<R>(f: impl FnOnce(&Worker) -> R) -> Option<R> {
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
    with_active(|w| w.cycle.get()).unwrap_or(0)
}

/// Clears the active Dispatcher unit slot when `ptr` is reclaimed and accounts for its quota once.
pub(crate) fn forget_held(ptr: *const ()) {
    let _ = with_active(|w| w.forget_held(ptr));
}

/// Per-worker state owning Cycle, Pull, quota, OPcache wait/reload, and active Dispatcher Unit.
pub(crate) struct Worker {
    slot: WorkerSlot,
    pool: Arc<Pool>,
    index: usize,
    workers: Arc<Workers>,
    opcache: Arc<OpcacheCoordinator>,
    max_requests: u64,
    /// Cycle counter on this worker thread (1 in the first cycle).
    cycle: Cell<u64>,
    /// Served units in the current app instance.
    served: Cell<u64>,
    /// True once the current app instance pulled at least one unit.
    received: Cell<bool>,
    /// Set once the current app instance takes no more units.
    end: Cell<Option<End>>,
    /// True until the first `handle_request()` call tears down the boot request.
    first_call: Cell<bool>,
    /// Set when the app booted with OPcache off; holds the `last_restart_time` to watch.
    opcache_wait: Cell<Option<zend_long>>,
    /// `last_restart_time` at the end of the previous app instance on this worker.
    previous_end: Cell<Option<zend_long>>,
    /// Restart reason saved by `opcache_restart_scheduled` while PHP holds the OPcache lock.
    opcache_scheduled: Cell<Option<c_int>>,
    /// True while this thread runs a host-initiated `opcache_reset()`.
    host_reset: Cell<bool>,
    /// Last Dispatcher unit handed to PHP, kept so bailouts can reclaim it.
    current_unit: Cell<Option<*mut dyn Held>>,
    /// True once `current_unit` has been counted against `served` and `max_requests`.
    current_unit_counted: Cell<bool>,
}

impl Worker {
    pub(crate) fn new(
        slot: WorkerSlot,
        pool: Arc<Pool>,
        index: usize,
        workers: Arc<Workers>,
        opcache: Arc<OpcacheCoordinator>,
        max_requests: u64,
    ) -> Self {
        Self {
            slot,
            pool,
            index,
            workers,
            opcache,
            max_requests: if max_requests == 0 {
                u64::MAX
            } else {
                max_requests
            },
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
    fn forget_held(&self, ptr: *const ()) {
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

    /// Reclaims a Dispatcher unit that `free_obj` never saw, for example after a shutdown or OOM bailout.
    fn reclaim_current(&self) {
        if let Some(ptr) = self.current_unit.take() {
            if !self.current_unit_counted.replace(true) {
                self.tick_quota();
            }
            // SAFETY: `ptr` came from `Box::leak` in `receive_into` and `current_unit` owned it.
            let st = unsafe { Box::from_raw(ptr) };
            if !st.settled() {
                self.pool
                    .record_worker(self.index, Event::Handled { errored: true });
            }
        }
    }

    fn has_active_unit(&self) -> bool {
        self.current_unit.get().is_some_and(|ptr| {
            // SAFETY: `current_unit` points to a live leaked `Held` box owned until `forget_held` or `reclaim_current`.
            unsafe { !(*ptr).is_finalized() }
        })
    }

    /// Verifies the previous Dispatcher unit is settled (discarding if cancelled) and counts its quota once.
    /// Returns false when the previous unit is still live and unsettled.
    fn settle_previous_unit(&self) -> bool {
        let Some(ptr) = self.current_unit.get() else {
            return true;
        };
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
            return false;
        }
        if !self.current_unit_counted.replace(true) {
            self.tick_quota();
        }
        true
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
        self.workers.wake();
        // https://github.com/php/php-src/blob/php-8.6.0/ext/opcache/ZendAccelerator.h#L105-L109
        let why = match reason {
            0 => "out of memory",
            1 => "hash table full",
            _ => "opcache_reset()",
        };
        warn!(target: "rapira", "OPcache scheduled a restart ({why}); every app restarts so that the restart can run");
    }

    /// Runs a standalone request calling `opcache_reset()` when a host reload requested one.
    #[must_use = "start the app or run the unit only after the reset request succeeds"]
    fn reset_opcache_before_request(&self) -> bool {
        let Some(sent) = self.opcache.claim_reset() else {
            return true;
        };
        let started = unsafe { php_request_startup() } == SUCCESS;
        if started {
            self.host_reset.set(true);
            unsafe { rapira_opcache_reset() };
            self.host_reset.set(false);
        }
        let shutdown = unsafe { rapira_request_shutdown() };
        self.opcache.finish_reset(sent, started);
        self.reload_for_opcache_restart();
        started && shutdown != RAPIRA_BAILOUT as c_int
    }

    /// Reads OPcache state once at app boot (requires an active PHP request).
    fn note_opcache_at_boot(&self) {
        let (state, last_restart) = opcache_state();
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

    /// True when the app booted with OPcache off and the pending restart ran (or the state read failed).
    fn opcache_restart_ran(&self) -> bool {
        self.opcache_wait.get().is_some_and(|seen| {
            let (state, last_restart) = opcache_state();
            // A read fails here only at the memory limit, and the app instance must end then.
            state < 0 || last_restart != seen
        })
    }

    /// Marks the worker idle across the pull and active before returning to PHP.
    #[inline]
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

    /// Pulls the next ready event, skipping cancelled units and keeping one deadline across OPcache polls.
    #[inline]
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
                self.slot.recv(Duration::ZERO)
            } else {
                let left = deadline.map_or(Duration::MAX, |d| {
                    d.saturating_duration_since(Instant::now())
                });
                let timeout = if self.opcache_wait.get().is_some() {
                    left.min(OPCACHE_POLL)
                } else {
                    left
                };
                self.slot.recv(timeout)
            };
            match &got {
                Pull::Unit(unit) if unit.cancelled() => {
                    self.pool.record_worker(self.index, Event::Shed);
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return Pull::NoUnit;
                    }
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
    #[inline]
    fn pull_timed(&self, wait: Wait) -> Pull {
        let mut disarmed = false;
        let got = self.pull_with(|w| {
            if !matches!(wait, Wait::Never) {
                let got = w.next_pull(Wait::Never);
                if !matches!(got, Pull::NoUnit) {
                    return got;
                }
            }
            unsafe { rapira_receive_untimed() };
            disarmed = true;
            w.next_pull(wait)
        });
        if !matches!(got, Pull::Unit(_)) && !disarmed {
            unsafe { rapira_receive_untimed() };
        }
        got
    }

    /// Classic mode: resolves the entrypoint and executes it once per pulled CGI unit.
    pub(crate) fn run_classic(&self, entrypoint: &Path) {
        let _active = ActiveWorkerGuard::enter(self);
        loop {
            resolve_script(entrypoint);
            clean_realpath_cache();
            loop {
                match self.pull_with(|w| w.next_pull(Wait::Forever)) {
                    Pull::Unit(unit) => {
                        let mut ctx = unit.into_cgi();
                        let errored = if !self.reset_opcache_before_request() {
                            error!(target: "rapira", "OPcache reset request failed; refusing the request");
                            ctx.refuse(500);
                            true
                        } else {
                            run_classic_unit(&mut ctx)
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

    /// Runs one resident app cycle (Worker or Dispatcher mode) and returns how it ended (`None` is a failed boot).
    fn run_cycle(&self, entrypoint: &Path) -> Option<End> {
        self.renew_quota();
        resolve_script(entrypoint);
        clean_realpath_cache();
        if !self.reset_opcache_before_request() {
            error!(target: "rapira", "OPcache reset request failed; restarting the app");
            self.reclaim_current();
            return self.outcome();
        }
        self.cycle.set(self.cycle.get() + 1);
        if boot_request_startup() {
            self.note_opcache_at_boot();
            unsafe { run_script(script().filename()) };
            // The boot of the next app instance compares `last_restart_time` with this value.
            let (state, last_restart) = opcache_state();
            self.previous_end.set((state >= 0).then_some(last_restart));
        } else {
            error!(target: "rapira", "php_request_startup() failed");
        }

        self.first_call.set(true);
        log_and_clear_last_error();
        let shutdown = unsafe { rapira_request_shutdown() };
        // A script or its shutdown functions can schedule a restart before leaving.
        self.reload_for_opcache_restart();
        if shutdown == RAPIRA_BAILOUT as c_int {
            error!(target: "rapira", "php_request_shutdown() bailed; restarting the app");
        }
        self.reclaim_current();
        self.outcome()
    }

    /// Resident loop (Worker and Dispatcher modes) with recycle, boot-failure backoff, and unit shedding.
    pub(crate) fn run_resident(&self, entrypoint: &Path, max_boot_failures: u32) {
        let _active = ActiveWorkerGuard::enter(self);
        let mut failures: u32 = 0;
        let mut retry_delay = BOOT_RETRY_BASE;
        'cycles: loop {
            let recycled = match self.run_cycle(entrypoint) {
                Some(End::Stop) => break,
                Some(End::Recycle) => true,
                None => false,
            };
            self.pool.record_worker(
                self.index,
                if recycled {
                    Event::Recycled
                } else {
                    Event::Starting
                },
            );
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
                crate::exit::exit(70);
            }
            // Units that arrive during the wait do not change the next boot time.
            let retry_at = Instant::now() + retry_delay;
            retry_delay = (retry_delay * 2).min(BOOT_RETRY_MAX);
            loop {
                match self.slot.pull_to_shed(retry_at) {
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
            if unsafe { rapira_request_teardown() } == RAPIRA_BAILOUT as c_int {
                error!(target: "rapira", "rapira_request_teardown() bailed on first call; recycling");
                self.mark_end(End::Recycle);
                return RAPIRA_HANDLE_RECYCLE;
            }
            // Hide boot shutdown functions from the per-unit pass; they run at cycle end.
            unsafe { rapira_stash_boot_shutdown_functions() };
        }
        log_and_clear_last_error();
        let mut ctx = match self.pull_timed(Wait::Forever) {
            Pull::Unit(unit) => unit.into_cgi(),
            Pull::Closed | Pull::Reload | Pull::NoUnit => return RAPIRA_HANDLE_STOP,
        };

        bind_server_context(&mut ctx);
        unsafe {
            populate_request_context(&mut ctx);
            rapira_release_temporary_streams();
        }
        let mut outcome = unsafe { rapira_request_activate() };
        if outcome != RAPIRA_BAILOUT as c_int {
            unsafe {
                crate::context::apply_proto_num(&ctx);
                outcome = rapira_run_handler(fci, fcc);
            }
        }
        ctx.begin_teardown();
        ctx.mark_errored(outcome != RAPIRA_OK as c_int);
        let teardown = unsafe { rapira_request_teardown() };

        let recycle = [outcome, teardown].contains(&(RAPIRA_BAILOUT as c_int));
        let errored = recycle || [outcome, teardown].contains(&(RAPIRA_THROW as c_int));

        log_and_clear_last_error();
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

    /// Dispatcher-mode `receive()` / `tryReceive()` core: verifies previous unit, pulls next unit, and writes its PHP object.
    /// # Safety
    /// `rv` is writable and the PHP engine is active on this thread.
    unsafe fn receive_into(&self, rv: *mut zval, mode: RecvMode) -> bool {
        if !self.settle_previous_unit() {
            // SAFETY: a dispatcher unit has a static plugin record and a static error message.
            unsafe { zend::throw_error(CStr::from_ptr((*rapira_current_plugin()).busy)) };
            return false;
        }
        let wait = match mode {
            RecvMode::Try | RecvMode::Wait(0) => Wait::Never,
            RecvMode::Wait(-1) => Wait::Forever,
            RecvMode::Wait(t) => Wait::For(Duration::from_micros(t as u64)),
        };
        unsafe {
            match self.pull_timed(wait) {
                Pull::Unit(unit) => {
                    rapira_receive_timed();
                    let ptr = NonNull::from(Box::leak(unit.into_held()));
                    self.current_unit.set(Some(ptr.as_ptr()));
                    self.current_unit_counted.set(false);
                    // A Zend OOM bailout in `rapira_unit_object` longjmps past this frame, so this frame holds no Drop values; `current_unit` reclaims the box.
                    rapira_unit_object(rv, ptr.as_ref().class())
                        .cast::<HeldPtr>()
                        .write(HeldPtr(Some(ptr)));
                    true
                }
                Pull::Closed | Pull::Reload => {
                    zend::throw_exception(rapira_ce_closed_exception, CLOSED_MSG);
                    false
                }
                Pull::NoUnit if matches!(mode, RecvMode::Try) => false,
                Pull::NoUnit => {
                    zend::throw_exception(rapira_ce_timeout_exception, TIMEOUT_MSG);
                    false
                }
            }
        }
    }
}

fn opcache_state() -> (c_int, zend_long) {
    let mut last_restart: zend_long = 0;
    let state = unsafe { rapira_opcache_state(&mut last_restart) };
    (state, last_restart)
}

/// Starts the boot request with `argv` of `php entrypoint.php` and builds `$_SERVER`, matching CLI.
/// After this, `argc` is 0 again so later `$_SERVER` builds do not reference a modified `$argv`.
/// https://github.com/php/php-src/blob/php-8.5.11/sapi/cli/php_cli.c#L941
/// https://github.com/php/php-src/blob/php-8.5.11/main/php_variables.c#L881-L887
fn boot_request_startup() -> bool {
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

/// Logs the last unmasked PHP diagnostic that `php_error_cb` did not log, then clears it.
fn log_and_clear_last_error() {
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

// `run_script` also returns false for exit() and die(), so only a recorded fatal error marks a failure.
fn run_classic_unit(ctx: &mut CgiExchange) -> bool {
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
        ctx.begin_teardown();
        ctx.mark_errored(exec_err);
        rapira_request_shutdown();
        // RSHUTDOWN seals inside a zend_try; a bailout there leaves the seal to this call.
        ctx.end(exec_err);
        exec_err
    };
    unbind_server_context();
    errored
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
/// Called from C (`rapira_finish_request`) on a worker thread inside an active request whose `CgiExchange` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_finish_response() {
    guard((), || unsafe {
        if let Some(c) = ctx() {
            // Shutdown functions, destructors, and teardown flush run after `mark_errored` is set.
            c.finish_response(fatal_error_recorded());
        }
    });
}

/// # Safety
/// `timeout_us >= -1`. `rv` is the return value of the receive shell, and the engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, rv: *mut zval) -> bool {
    guard(false, || {
        with_active(|w| unsafe { w.receive_into(rv, RecvMode::Wait(timeout_us)) }).unwrap_or_else(
            || {
                unsafe { zend::throw_exception(rapira_ce_closed_exception, CLOSED_MSG) };
                false
            },
        )
    })
}

/// # Safety
/// As `rapira_rs_receive`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(rv: *mut zval) -> bool {
    guard(false, || {
        with_active(|w| unsafe { w.receive_into(rv, RecvMode::Try) }).unwrap_or_else(|| {
            unsafe { zend::throw_exception(rapira_ce_closed_exception, CLOSED_MSG) };
            false
        })
    })
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
mod tests;
