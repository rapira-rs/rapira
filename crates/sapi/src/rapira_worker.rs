use tracing::error;

use crate::{
    callbacks::*,
    diagnostics::error_type_to_level,
    scoreboard::sb_update,
    start::{Pulled, pull_job, pull_job_to_shed, run_script},
};
use std::{
    cell::Cell,
    ffi::CString,
    os::raw::c_int,
    path::{Path, PathBuf},
    ptr::null_mut,
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
}

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
    let arg0 = CString::new(crate::context::script().filename.as_bytes()).unwrap_or_default();
    let mut argv = [arg0.as_ptr().cast_mut()];
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
    let started = boot_request_startup();
    if started {
        unsafe { run_script(script) };
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

    if crate::exchange::CLOSED_SEEN.get() {
        Cycle::Stop
    } else if recycle || crate::exchange::RECEIVED.get() {
        Cycle::Recycle
    } else {
        Cycle::Failed
    }
}

pub fn rapira_worker(script: PathBuf) -> WorkerExit {
    let mut failures: u32 = 0;
    let exit = loop {
        match run_cycle(&script) {
            Cycle::Stop => break WorkerExit::Closed,
            Cycle::Restart => break WorkerExit::Restart,
            Cycle::Recycle => {
                sb_update(scoreboard::Event::Recycled);
                failures = 0;
            }
            Cycle::Failed => {
                // we should probably adopt here circuit breaker algorithm
                failures += 1;
                if failures == UNHEALTHY_AFTER {
                    error!(target: "rapira", "worker keeps failing to boot; flagged unhealthy");
                    sb_update(scoreboard::Event::Unhealthy);
                }
                sb_update(scoreboard::Event::BootFailed);
                match pull_job_to_shed(BOOT_RETRY) {
                    Pulled::Closed => break WorkerExit::Closed,
                    Pulled::Job(unit) => {
                        unit.shed();
                        sb_update(scoreboard::Event::Shed);
                    }
                    Pulled::Timeout | Pulled::Empty => {}
                }
            }
        }
    };
    crate::exchange::reclaim_current();
    exit
}

/// # Safety
/// Called from the C handle_request shell on the resident worker thread inside its active request, with valid `fci`/`fcc`. The unbind here is the only one per job. It also covers the caught-panic path, where SG(server_context) would otherwise point at a freed job.
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
    let Some(mut ctx) = next_job() else {
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
    let teardown = unsafe { rapira_request_teardown() };

    let recycle: bool = [outcome, flushed, teardown].contains(&(RAPIRA_BAILOUT as c_int));
    let errored: bool = recycle || outcome == RAPIRA_THROW as c_int;
    let truncated: bool = finalize_response(&mut ctx, errored);

    log_and_clear_last_error();
    sb_update(scoreboard::Event::Handled(errored));
    if recycle {
        RECYCLE.set(true);
    }
    ctx.finish(truncated);
    if recycle {
        RAPIRA_HANDLE_RECYCLE
    } else {
        RAPIRA_HANDLE_CONTINUE
    }
}

/// The first call tears down the bootstrap request php_request_startup() left behind, before any job is served.
fn next_job() -> Option<Context> {
    if FIRST_CALL.take() {
        if unsafe { rapira_request_teardown() } == RAPIRA_BAILOUT as c_int {
            error!(target: "rapira", "rapira_request_teardown() bailed on first call; recycling");
            RECYCLE.set(true);
            return None;
        }
        // hide boot registrations from the per-job shutdown pass; they run at cycle end
        unsafe { rapira_stash_boot_shutdown_functions() };
    }
    log_and_clear_last_error();
    loop {
        match pull_job() {
            Some(unit) => {
                if unit.cancelled() {
                    sb_update(scoreboard::Event::Handled(true));
                    continue;
                }
                crate::exchange::RECEIVED.set(true);
                let Some(ctx) = unit.into_cgi() else {
                    unreachable!("a unit with no CGI form goes to dispatcher-mode workers only");
                };
                return Some(ctx);
            }
            None => {
                crate::exchange::CLOSED_SEEN.set(true);
                return None;
            }
        }
    }
}

/// # Safety
/// Called from C (`rapira_finish_request`) on a worker thread inside an active request whose `Context` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_finish_response() {
    guard((), || unsafe {
        if let Some(c) = ctx() {
            c.finish(false);
        }
    });
}

/// clear_last_error() keeps last_error_type/lineno set (main/main.c:1307-1316) so the message pointer is the only "slot filled" signal, and php_error_cb applies EG(error_reporting) only after filling it (main/main.c:1394-1411).
fn log_and_clear_last_error() {
    unsafe {
        let msg = core_globals.last_error_message;
        if !msg.is_null() {
            let lineno = core_globals.last_error_lineno;
            let (level, label) = error_type_to_level(
                core_globals.last_error_type,
                executor_globals.error_reporting,
            );
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
