use tracing::{error, info, warn};

use crate::{
    callbacks::*,
    diagnostics::{Severity, error_type_to_level},
    scoreboard::sb_update,
    start::{Pull, Wait, pull, pull_unit_to_shed, run_script},
};
use std::{
    cell::Cell,
    os::raw::c_int,
    path::Path,
    ptr::null_mut,
    sync::atomic::{AtomicI32, Ordering},
    time::Duration,
};

use crate::{
    callbacks::guard,
    context::{bind_server_context, ctx, populate_request_context, unbind_server_context},
    php_request_startup, rapira_run_handler,
    types::Context,
    zend_fcall_info, zend_fcall_info_cache, *,
};

thread_local! {
    /// The next handle_request() call tears down the boot request first.
    static FIRST_CALL: Cell<bool> = const { Cell::new(true) };
    static RECYCLE: Cell<bool> = const { Cell::new(false) };
    /// The app instance runs with OPcache off until the OPcache restart has run: the last_restart_time that the pull waits to see change.
    pub(crate) static OPCACHE_WAIT: Cell<Option<zend_long>> = const { Cell::new(None) };
    /// The last_restart_time at the end of the previous app instance of this thread.
    static PREVIOUS_END: Cell<Option<zend_long>> = const { Cell::new(None) };
    /// The reload count of the server, set before the first app boot.
    pub(crate) static RELOADS: Cell<Option<&'static rapira_scoreboard::Reloads>> = const { Cell::new(None) };
}

/// The zend_accel_restart_reason of a restart that OPcache scheduled in this process, until the next pull reports it.
static OPCACHE_SCHEDULED: AtomicI32 = AtomicI32::new(NO_RESTART);
const NO_RESTART: i32 = -1;

/// The flags of rapira_opcache_state().
const OPCACHE_ON: c_int = 1;
const OPCACHE_PENDING: c_int = 2;
const OPCACHE_RESTARTING: c_int = 4;

const UNHEALTHY_AFTER: u32 = 5;
/// After a failed boot cycle, the worker runs the boot again when no unit arrives within this time. A load balancer that reads `/readyz` sends no requests to a pool that is not ready.
const BOOT_RETRY: Duration = Duration::from_secs(5);

enum Cycle {
    Stop,
    Recycle,
    Failed,
    Restart,
}

pub enum WorkerExit {
    Closed,
    Restart,
}

/// Starts the boot request with the argv of `php entrypoint.php` and builds $_SERVER before the script runs, as the CLI does.
/// argc is 0 again after this: with argc set, each later $_SERVER build adds a reference to the $argv global, and that crashes when $argv holds null, an int or [].
/// https://github.com/php/php-src/blob/php-8.5.11/sapi/cli/php_cli.c#L941
/// https://github.com/php/php-src/blob/php-8.5.11/main/php_variables.c#L881-L887
fn boot_request_startup() -> bool {
    // php_build_argv copies the string; the script paths stay until the next app start.
    let mut argv = [crate::context::script().filename.as_ptr().cast_mut()];
    unsafe {
        let sg = &raw mut sapi_globals;
        (*sg).request_info.argc = 1;
        (*sg).request_info.argv = argv.as_mut_ptr();
        // PHP 8.4 adds argv only with register_argc_argv on, so its CLI and embed SAPIs force it on; PHP 8.5 adds argv when argc is set.
        // https://github.com/php/php-src/blob/php-8.4.26/sapi/embed/php_embed.c#L26-L32
        #[cfg(php84)]
        let register_argc_argv = std::ptr::replace(&raw mut core_globals.register_argc_argv, true);
        let started = php_request_startup() == SUCCESS;
        if started {
            zend_is_auto_global_str(c"_SERVER".as_ptr(), c"_SERVER".count_bytes());
        }
        #[cfg(php84)]
        {
            core_globals.register_argc_argv = register_argc_argv;
        }
        (*sg).request_info.argc = 0;
        (*sg).request_info.argv = null_mut();
        started
    }
}

/// Logs PG(last_error_message) before php_request_shutdown frees it (main/main.c:2024).
fn run_cycle(script: &Path) -> Cycle {
    crate::exchange::cycle_reset();
    crate::quota::renew();
    crate::context::resolve_script(script);
    crate::context::enter_entrypoint_dir();
    if !reset_opcache_before_request() {
        error!(target: "rapira", "php_request_shutdown() bailed; restarting the PHP thread");
        return Cycle::Restart;
    }
    let started = boot_request_startup();
    if started {
        note_opcache_at_boot();
        unsafe { run_script(&crate::context::script().filename) };
        // the boot of the next app instance compares last_restart_time with this value
        let mut last_restart: zend_long = 0;
        let state = unsafe { rapira_opcache_state(&mut last_restart) };
        PREVIOUS_END.set((state >= 0).then_some(last_restart));
        reset_opcache_if_due();
    } else {
        error!(target: "rapira", "php_request_startup() failed");
    }

    FIRST_CALL.set(true);
    let recycle = RECYCLE.take();

    log_and_clear_last_error();
    if unsafe { rapira_request_shutdown() } == RAPIRA_BAILOUT as c_int {
        error!(target: "rapira", "php_request_shutdown() bailed; restarting the PHP thread");
        return Cycle::Restart;
    }

    if crate::start::CLOSED_SEEN.get() {
        Cycle::Stop
    } else if recycle || crate::quota::exhausted() || crate::start::RECEIVED.get() {
        Cycle::Recycle
    } else {
        Cycle::Failed
    }
}

/// What an app boot does about OPcache.
enum OpcacheBoot {
    Run,
    /// OPcache is on and a restart is pending: this app instance holds the usage lock, so it leaves at its first pull.
    LeaveAtFirstPull,
    /// OPcache is off for this app instance: it leaves when last_restart_time is no longer `seen`.
    Wait {
        seen: zend_long,
    },
}

/// The boot decision from the result of rapira_opcache_state() and the last_restart_time at the end of the previous app instance.
/// An app instance that boots while a restart is pending has OPcache off for its whole life, and the restart runs only when no app holds the usage lock (ZEND_RINIT_FUNCTION(zend_accelerator) in ext/opcache/ZendAccelerator.c).
/// The restart clears restart_pending before it turns OPcache on, so a boot with OPcache off also waits when a restart runs now or ran after the previous app instance.
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

/// Reads the OPcache state one time at the app boot. Needs an active request.
fn note_opcache_at_boot() {
    let mut last_restart: zend_long = 0;
    let state = unsafe { rapira_opcache_state(&mut last_restart) };
    OPCACHE_WAIT.set(None);
    match opcache_boot(state, last_restart, PREVIOUS_END.get()) {
        OpcacheBoot::Run => {}
        OpcacheBoot::LeaveAtFirstPull => crate::quota::exhaust(),
        OpcacheBoot::Wait { seen } => {
            info!(target: "rapira", "the app booted with OPcache off; it restarts when the OPcache restart has run");
            OPCACHE_WAIT.set(Some(seen));
        }
    }
}

/// The app runs with OPcache off, and the restart that it waits for has run or the state read failed. Needs an active request.
pub(crate) fn opcache_restart_ran() -> bool {
    OPCACHE_WAIT.get().is_some_and(|seen| {
        let mut last_restart: zend_long = 0;
        let state = unsafe { rapira_opcache_state(&mut last_restart) };
        // a read fails here only at the memory limit, and the app instance must end then
        state < 0 || last_restart != seen
    })
}

/// OPcache calls it on the PHP thread that schedules a restart, inside OPcache with its lock held and before it marks the restart pending, so it only stores the reason.
/// It stores nothing while the reset of a reload is due: every app restarts for that reload, so the restart can run with no report.
pub(crate) unsafe extern "C" fn opcache_restart_scheduled(reason: c_int) {
    if RELOADS.get().is_some_and(|r| r.reset_due().is_some()) {
        return;
    }
    OPCACHE_SCHEDULED.store(reason, Ordering::Relaxed);
}

/// The restart runs only when no app holds OPcache, so the master reloads every pool. SIGUSR2 is the reload of the master, the parent of every worker.
pub(crate) fn reload_for_opcache_restart() {
    // a load first: almost every pull finds no record, and a swap is a locked instruction
    if OPCACHE_SCHEDULED.load(Ordering::Relaxed) == NO_RESTART {
        return;
    }
    let reason = OPCACHE_SCHEDULED.swap(NO_RESTART, Ordering::Relaxed);
    // the order of zend_accel_restart_reason (ext/opcache/ZendAccelerator.h)
    let why = match reason {
        0 => "out of memory",
        1 => "hash table full",
        _ => "opcache_reset()",
    };
    warn!(target: "rapira", "OPcache scheduled a restart ({why}); every app restarts so that the restart can run");
    // SAFETY: plain syscalls.
    unsafe { libc::kill(libc::getppid(), libc::SIGUSR2) };
}

/// When a reset is due, makes the reset call in a request of its own that runs no script. OPcache takes the usage lock at the first cached compile of a request (persistent_compile_file in ext/opcache/ZendAccelerator.c), and a restart that runs before that lock empties the strings of a script that the request then loads. The next request starts with the restart pending or done.
/// Returns false when the shutdown of that request bailed.
pub(crate) fn reset_opcache_before_request() -> bool {
    // Read before the request starts: when no reset is due, a reset call for every reload so far has returned.
    if RELOADS.get().is_none_or(|r| r.reset_due().is_none()) {
        return true;
    }
    unsafe {
        if php_request_startup() == SUCCESS {
            reset_opcache_if_due();
        }
        rapira_request_shutdown() != RAPIRA_BAILOUT as c_int
    }
}

/// A reload empties OPcache, as a PHP-FPM reload does. A worker calls opcache_reset() only when no reset call for the last reload has returned, and it records its call after the call. A call that finds a restart pending schedules nothing.
/// A call from a request that holds no usage lock can schedule a second restart for one reload: it waits for the lock of OPcache while another call schedules the first restart and a third request runs it. opcache_restart_scheduled keeps the record of that second restart, so the next pull asks the master for a reload.
/// Needs an active request: the end of each app instance in worker and dispatcher mode, and the request of reset_opcache_before_request in every mode.
fn reset_opcache_if_due() {
    let Some(reloads) = RELOADS.get() else {
        return;
    };
    if let Some(sent) = reloads.reset_due() {
        unsafe { rapira_opcache_reset() };
        reloads.record_reset(sent);
    }
}

pub fn rapira_worker(script: &Path) -> WorkerExit {
    let mut failures: u32 = 0;
    let exit = loop {
        match run_cycle(script) {
            Cycle::Stop => break WorkerExit::Closed,
            Cycle::Restart => break WorkerExit::Restart,
            Cycle::Recycle => {
                sb_update(scoreboard::Event::Recycled);
                // No request runs while the app boots again, so the request watchdog skips the slot.
                sb_update(scoreboard::Event::Idle);
                failures = 0;
            }
            Cycle::Failed => {
                failures += 1;
                if failures == UNHEALTHY_AFTER {
                    error!(target: "rapira", "worker keeps failing to boot; flagged unhealthy");
                    crate::quota::fire_unhealthy();
                }
                sb_update(scoreboard::Event::BootFailed);
                match pull_unit_to_shed(BOOT_RETRY) {
                    Pull::Closed => break WorkerExit::Closed,
                    Pull::Unit(unit) => {
                        unit.shed();
                        sb_update(scoreboard::Event::Shed);
                    }
                    // The retry time passed, or a reload arrived: the next boot starts.
                    Pull::NoUnit => {}
                }
            }
        }
    };
    crate::exchange::reclaim_current();
    exit
}

/// # Safety
/// Called from the C handle_request shell on the resident worker thread inside its active request, with valid `fci`/`fcc`. The unbind here is the only one per unit. It also covers the caught-panic path, where SG(server_context) would otherwise point at a freed unit.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_handle_request(
    fci: *mut zend_fcall_info,
    fcc: *mut zend_fcall_info_cache,
) -> c_int {
    let action = guard(RAPIRA_HANDLE_RECYCLE, || handle_request_impl(fci, fcc));
    unbind_server_context();
    action as c_int
}

/// Flushes before rapira_request_teardown: the real head (status, cookies, php_error_cb's 500) lives in SG(sapi_headers), which teardown destroys.
fn handle_request_impl(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> u32 {
    let Some(mut ctx) = next_unit() else {
        return if RECYCLE.get() {
            RAPIRA_HANDLE_RECYCLE
        } else {
            RAPIRA_HANDLE_STOP
        };
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

    ctx.tearing_down = true;
    let flushed = if outcome == RAPIRA_OK as c_int {
        RAPIRA_OK as c_int
    } else {
        unsafe { rapira_finish_output() }
    };
    ctx.errored = outcome != RAPIRA_OK as c_int || flushed == RAPIRA_BAILOUT as c_int;
    let teardown = unsafe { rapira_request_teardown() };

    let recycle: bool = [outcome, flushed, teardown].contains(&(RAPIRA_BAILOUT as c_int));
    let errored: bool = recycle || outcome == RAPIRA_THROW as c_int;

    log_and_clear_last_error();
    sb_update(scoreboard::Event::Handled { errored });
    if recycle {
        RECYCLE.set(true);
        RAPIRA_HANDLE_RECYCLE
    } else {
        RAPIRA_HANDLE_CONTINUE
    }
}

/// The first call tears down the bootstrap request php_request_startup() left behind, before any unit is served.
fn next_unit() -> Option<Box<Context>> {
    if FIRST_CALL.take() {
        if unsafe { rapira_request_teardown() } == RAPIRA_BAILOUT as c_int {
            error!(target: "rapira", "rapira_request_teardown() bailed on first call; recycling");
            RECYCLE.set(true);
            return None;
        }
        // hide boot registrations from the per-unit shutdown pass; they run at cycle end
        unsafe { rapira_stash_boot_shutdown_functions() };
    }
    log_and_clear_last_error();
    match pull(Wait::Forever) {
        Pull::Unit(unit) => Some(unit.into_cgi()),
        // The app instance takes no more units: handle_request() returns false, and the worker stays.
        Pull::Closed | Pull::NoUnit => None,
    }
}

/// # Safety
/// Called from C (`rapira_finish_request`) on a worker thread inside an active request whose `Context` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_finish_response() {
    guard((), || unsafe {
        if let Some(c) = ctx() {
            // shutdown functions, destructors and the teardown flush run after ctx.errored is set
            let errored = c.errored || fatal_error_recorded();
            c.end(errored);
        }
    });
}

/// clear_last_error() keeps last_error_type/lineno set (main/main.c:1307-1316) so the message pointer is the only "slot filled" signal, and php_error_cb applies EG(error_reporting) only after filling it (main/main.c:1394-1411).
/// The record covers the last diagnostic that php_error_cb did not log: EG(error_reporting) at this call masks its type, log_errors is 0, or error_log is set.
/// The check reads the mask at this call. When @ or a temporary error_reporting() call masked the diagnostic and the code restored the mask before this call, the diagnostic gets no record. PHP does not log it either.
fn log_and_clear_last_error() {
    unsafe {
        let msg = core_globals.last_error_message;
        let mask = executor_globals.error_reporting;
        // php_error_cb logs an unmasked diagnostic through log_message when log_errors is set and error_log is empty (main/main.c:1410-1463, 935-988)
        let php_logged = core_globals.last_error_type & (mask | E_CORE as c_int) != 0
            && core_globals.log_errors
            && core_globals.error_log.is_null();
        if !msg.is_null() && !php_logged {
            let lineno = core_globals.last_error_lineno;
            let Severity { level, label } = error_type_to_level(core_globals.last_error_type, mask);
            crate::diagnostics::event_at!(
                "php",
                level,
                "{label}: {} in {}:{}",
                String::from_utf8_lossy(zend::zstr_bytes(msg)),
                String::from_utf8_lossy(zend::zstr_bytes(core_globals.last_error_file)),
                lineno
            );
        }
        rapira_clear_last_error();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            /// The result of rapira_opcache_state().
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
            // Known limit: a boot inside a restart can read this state, and the app then runs without OPcache until its next start.
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
    fn the_restart_hook_stores_no_reason_while_a_reset_is_due() {
        struct Case {
            name: &'static str,
            reload_sent: bool,
            reset_recorded: bool,
            /// OPCACHE_SCHEDULED after the hook ran with reason 2.
            want: i32,
        }
        let cases = [
            Case {
                name: "no reload",
                reload_sent: false,
                reset_recorded: false,
                want: 2,
            },
            Case {
                name: "a reload, and no reset call has returned",
                reload_sent: true,
                reset_recorded: false,
                want: NO_RESTART,
            },
            Case {
                name: "a reload, and a reset call has returned",
                reload_sent: true,
                reset_recorded: true,
                want: 2,
            },
        ];
        for case in &cases {
            let reloads = rapira_scoreboard::create_reloads().expect("map the reload count");
            if case.reload_sent {
                reloads.add_sent();
            }
            if case.reset_recorded {
                reloads.record_reset(reloads.reset_due().expect("a reset is due"));
            }
            RELOADS.set(Some(reloads));
            OPCACHE_SCHEDULED.store(NO_RESTART, Ordering::Relaxed);
            unsafe { opcache_restart_scheduled(2) };
            assert_eq!(
                OPCACHE_SCHEDULED.load(Ordering::Relaxed),
                case.want,
                "{}",
                case.name
            );
        }
        OPCACHE_SCHEDULED.store(NO_RESTART, Ordering::Relaxed);
    }
}
