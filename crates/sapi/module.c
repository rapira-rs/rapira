#include "rapira_sapi.h"

#include <main/php_content_types.h>
#include <time.h>
#ifdef __linux__
#include <sys/syscall.h>
#endif

// True while rapira_run_handler runs the handler and its shutdown functions.
ZEND_TLS bool rapira_in_handler = false;

// php_handle_aborted_connection (main.c:2722) longjmps past Rust's
// catch_unwind. In worker mode a bailout runs the boot again, so the abort ends
// only the unit. The handler stops as on exit(), and the teardown drops the
// output.
extern size_t rapira_rs_ub_write(const char *str, size_t len, bool *aborted);
size_t rapira_ub_write(const char *str, size_t len) {
    bool aborted = false;
    size_t written = rapira_rs_ub_write(str, len, &aborted);
    if (aborted && rapira_current_mode() != RAPIRA_MODE_WORKER) {
        php_handle_aborted_connection();
    } else if (aborted) {
        PG(connection_status) = PHP_CONNECTION_ABORTED;
        php_output_set_status(PHP_OUTPUT_DISABLED);
        if (rapira_in_handler && !PG(ignore_user_abort) && !EG(exception)) {
            zend_throw_unwind_exit();
        }
    }
    return written;
}

// Sets the flag on a bailout and closes the observer frames the longjmp skips.
#define RAPIRA_GUARD(stmt, flag, base)                                         \
    zend_try { stmt; }                                                         \
    zend_catch {                                                               \
        (flag) = RAPIRA_BAILOUT;                                               \
        rapira_observer_end_to(base);                                          \
    }                                                                          \
    zend_end_try()

// Like RAPIRA_GUARD, but records no outcome. It only closes observer frames.
#define RAPIRA_OBSERVER_CLOSE(stmt, base)                                      \
    zend_try { stmt; }                                                         \
    zend_catch { rapira_observer_end_to(base); }                               \
    zend_end_try()

// php_output_end_all flushes the output handlers of the app. A handler can
// raise a fatal error, which bails out.
int rapira_finish_output(void) {
    zend_try {
        php_output_end_all();
        php_header(); // does nothing when SG(headers_sent) is true
    }
    zend_catch { return RAPIRA_BAILOUT; }
    zend_end_try();

    return RAPIRA_OK;
}

PHP_FUNCTION(rapira_finish_request) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (rapira_current_mode() == RAPIRA_MODE_DISPATCHER) {
        // without this check it would flush the app ob buffers into the log
        zend_throw_error(
            NULL, "rapira_finish_request() is not available in dispatcher "
                  "mode; finalize the unit that receive() returned");
        RETURN_THROWS();
    }
    if (rapira_finish_output() != RAPIRA_OK) {
        // raise the bailout again. rapira_run_handler catches it in worker
        // mode, and php_execute_script catches it in classic mode
        zend_bailout();
    }
    rapira_rs_finish_response();
    RETURN_TRUE;
}

// ext/filter frees its cached raw input only in RSHUTDOWN (filter.c:190-196).
// PECL imap frees its error and alert stacks only in RSHUTDOWN. Without the
// RSHUTDOWN and RINIT calls per unit, a resident worker leaks, and unit N reads
// the errors of unit N-1.
static const char *RELOAD_MODULES[] = {"filter", "imap", NULL};

static void rapira_modules_request(bool startup) {
    zend_module_entry *module = NULL;
    for (const char **name = RELOAD_MODULES; *name; name++) {
        module = zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
        if (!module) {
            continue;
        }
        if (startup && module->request_startup_func) {
            // a RINIT failure is fatal in php-src, because
            // zend_activate_modules calls exit(1)
            if (module->request_startup_func(
                    module->type, module->module_number) == FAILURE) {
                zend_error(E_WARNING, "request_startup() for %s module failed",
                           module->name);
                zend_bailout();
            }
        } else if (!startup && module->request_shutdown_func) {
            module->request_shutdown_func(module->type, module->module_number);
        }
    }
}

// Closes only the frames above base, because end_all() would also close the
// resident frames.
static void rapira_observer_end_to(zend_execute_data *base) {
    if (!ZEND_OBSERVER_ENABLED) {
        return;
    }
    zend_execute_data *orig = EG(current_execute_data);
    while (EG(current_observed_frame) && EG(current_observed_frame) != base) {
        EG(current_execute_data) = EG(current_observed_frame);
        zend_observer_fcall_end_prechecked(EG(current_observed_frame), NULL);
    }
    EG(current_execute_data) = orig;
}

// The per-unit time budget, or -1 when it is not captured. set_time_limit()
// rewrites the live field.
ZEND_TLS zend_long rapira_unit_timeout = -1;
// True when the kernel execution timer is currently armed for
// rapira_unit_timeout.
ZEND_TLS bool rapira_timer_armed = false;
// Coarse monotonic timestamp (ms) when the kernel timer was last armed.
ZEND_TLS int64_t rapira_timer_armed_ms = 0;

static int64_t rapira_coarse_ms(void) {
    struct timespec ts;
#ifdef CLOCK_MONOTONIC_COARSE
    if (clock_gettime(CLOCK_MONOTONIC_COARSE, &ts) == 0) {
        return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
    }
#endif
    if (clock_gettime(CLOCK_MONOTONIC, &ts) == 0) {
        return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
    }
    return 0;
}

// zend_unset_timeout does not stop the timer when EG(timeout_seconds) is 0, so
// stop the timer before this function sets the field to 0.
void rapira_receive_untimed(void) {
    if (rapira_unit_timeout < 0) {
        rapira_unit_timeout = EG(timeout_seconds);
    }
    if (rapira_timer_armed || EG(timeout_seconds) != 0) {
        zend_unset_timeout();
        EG(timeout_seconds) = 0;
        rapira_timer_armed = false;
    }
}

// zend_set_timeout sets EG(timeout_seconds) itself (zend_execute_API.c).
// receive() calls this without rapira_receive_untimed() when a unit is queued.
// Callers: receive() for each pulled unit, and rapira_request_init for each
// worker-mode unit.
void rapira_receive_timed(void) {
    if (rapira_unit_timeout < 0) {
        rapira_unit_timeout = EG(timeout_seconds);
    }
    if (rapira_unit_timeout == 0) {
        if (rapira_timer_armed || EG(timeout_seconds) != 0) {
            zend_unset_timeout();
            EG(timeout_seconds) = 0;
            rapira_timer_armed = false;
        }
        return;
    }
    // Re-arm on the first unit, after an idle wait, after set_time_limit(), or
    // once at least 1% of the budget (capped at 100 ms) has elapsed. Skipping
    // redundant timer_settime() calls avoids kernel sighand->siglock contention
    // across worker threads on fast requests.
    int64_t now_ms = rapira_coarse_ms();
    int64_t rearm_ms = rapira_unit_timeout * 10;
    if (rearm_ms > 100) {
        rearm_ms = 100;
    }
    if (rapira_timer_armed && EG(timeout_seconds) == rapira_unit_timeout &&
        (now_ms - rapira_timer_armed_ms) < rearm_ms) {
        return;
    }
    zend_set_timeout(rapira_unit_timeout, false);
    rapira_timer_armed = true;
    rapira_timer_armed_ms = now_ms;
}

static bool rapira_timeouts_disabled;

bool rapira_timer_pause(rapira_timer *saved) {
#if defined(ZEND_MAX_EXECUTION_TIMERS)
    // zend_max_execution_timer_settime uses the same check for a missing timer.
    if (!EG(pid)) {
        return false;
    }
    // Leave an expired timer unchanged. Linux can discard its pending signal if
    // the timer changes.
    // https://github.com/torvalds/linux/blob/v7.3/kernel/time/posix-timers.c#L305-L311
    struct itimerspec now = {0};
    if (timer_gettime(EG(max_execution_timer_timer), &now) != 0 ||
        (now.it_value.tv_sec == 0 && now.it_value.tv_nsec == 0)) {
        rapira_timer_armed = false;
        return false;
    }
    struct itimerspec off = {0};
    if (timer_settime(EG(max_execution_timer_timer), 0, &off, saved) != 0) {
        rapira_timer_armed = false;
        return false;
    }
    if (saved->it_value.tv_sec == 0 && saved->it_value.tv_nsec == 0) {
        // The timer expired between timer_gettime and timer_settime; re-arm
        // 1 ns so the timeout signal still arrives if Linux discarded it.
        rapira_timer_armed = false;
        struct itimerspec fire = {.it_value = {.tv_sec = 0, .tv_nsec = 1}};
        timer_settime(EG(max_execution_timer_timer), 0, &fire, NULL);
        return false;
    }
    return true;
#elif defined(HAVE_SETITIMER)
    if (rapira_timeouts_disabled) {
        return false;
    }
    // One call reads and stops the timer.
    // https://man7.org/linux/man-pages/man2/setitimer.2.html
    struct itimerval off = {0};
    if (setitimer(RAPIRA_ZEND_ITIMER, &off, saved) != 0) {
        rapira_timer_armed = false;
        return false;
    }
    if (saved->it_value.tv_sec == 0 && saved->it_value.tv_usec == 0) {
        rapira_timer_armed = false;
        return false;
    }
    return true;
#else
    (void)saved;
    return false;
#endif
}

void rapira_timer_resume(const rapira_timer *saved) {
    // Force the next unit to re-arm its full budget after a paused write.
    rapira_timer_armed = false;
#if defined(ZEND_MAX_EXECUTION_TIMERS)
    timer_settime(EG(max_execution_timer_timer), 0, saved, NULL);
#elif defined(HAVE_SETITIMER)
    setitimer(RAPIRA_ZEND_ITIMER, saved, NULL);
#else
    (void)saved;
#endif
}

#if defined(ZEND_MAX_EXECUTION_TIMERS) && defined(__linux__)
// glibc has no name for this field.
// https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_max_execution_timer.c#L36-L40
#ifndef sigev_notify_thread_id
#define sigev_notify_thread_id _sigev_un._tid
#endif
#endif

// True when the pool of this thread sets execution_clock to cpu.
ZEND_TLS bool rapira_cpu_clock = false;

// Zend creates the execution timer of this thread on a wall clock in each
// request startup.
// https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_max_execution_timer.c#L49-L85
// For a pool with the cpu clock, this function moves the timer to the CPU clock
// of the thread. https://man7.org/linux/man-pages/man2/timer_create.2.html
void rapira_timer_use_cpu_clock(void) {
    if (!rapira_cpu_clock) {
        return;
    }
#if defined(ZEND_MAX_EXECUTION_TIMERS) && defined(__linux__)
    struct sigevent sev = {0};
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo = RAPIRA_TIMER_SIGNAL;
    // zend_timeout_handler accepts only a signal with this value.
    // https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_execute_API.c#L1535
    sev.sigev_value.sival_ptr = &EG(max_execution_timer_timer);
    sev.sigev_notify_thread_id = (pid_t)syscall(SYS_gettid);
    timer_t cpu;
    if (timer_create(CLOCK_THREAD_CPUTIME_ID, &sev, &cpu) == 0) {
        timer_delete(EG(max_execution_timer_timer));
        EG(max_execution_timer_timer) = cpu;
        // Arms the full budget of php_request_startup on the new timer, and
        // clears a timeout that the wall clock recorded before RINIT.
        // https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_execute_API.c#L1724-L1730
        zend_set_timeout(EG(timeout_seconds), false);
    }
#endif
}

// Resets the per-request state as php_request_startup() does, because the
// worker path skips that function.
static void rapira_request_init(void) {
    PG(connection_status) = PHP_CONNECTION_NORMAL;
    PG(header_is_being_sent) = 0;
    // a fatal error can leave these flags set, which breaks later URL opens and
    // error logging
    PG(in_error_log) = false;
    PG(in_user_include) = false;
    // init_compiler clears this per cycle (zend_compile.c:461), not per unit
    CG(unclean_shutdown) = false;

    // Reuse the timer signal handler that php_request_startup installed for
    // this cycle.
    rapira_receive_timed();

    if (PG(expose_php)) {
        // php_version() is the PHP_VERSION of the loaded libphp, which can be
        // another patch release than the build headers (main/php_main.h)
        char line[128];
        int len =
            slprintf(line, sizeof(line), "X-Powered-By: PHP/%s", php_version());
        sapi_add_header(line, len, 1);
    }

    // from PHP 8.6, output_handler is a zend_string*, and an empty value is
    // NULL (php-src e0221be8)
#if PHP_VERSION_ID >= 80600
    if (PG(output_handler)) {
        zval oh;
        ZVAL_STR_COPY(&oh, PG(output_handler));
#else
    if (PG(output_handler) && PG(output_handler)[0]) {
        zval oh;
        ZVAL_STRING(&oh, PG(output_handler));
#endif
        php_output_start_user(&oh, 0, PHP_OUTPUT_HANDLER_STDFLAGS);
        zval_ptr_dtor(&oh);
    } else if (PG(output_buffering)) {
        php_output_start_user(
            NULL, PG(output_buffering) > 1 ? PG(output_buffering) : 0,
            PHP_OUTPUT_HANDLER_STDFLAGS);
    } else if (PG(implicit_flush)) {
        php_output_set_implicit_flush(1);
    }
}

// php_hash_environment re-arms CG(auto_globals) per request; the per-unit path
// skips it.
static void rapira_activate_auto_globals(void) {
    zend_auto_global *auto_global = NULL;
    zend_string *_env = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_ENV);

    // skip $_ENV, because its create callback destroys the array before it
    // checks variables_order
    ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), auto_global) {
        if (auto_global->name == _env) {
            continue;
        }
        auto_global->armed =
            ((auto_global->jit || auto_global->auto_global_callback) != 0);
    }
    ZEND_HASH_FOREACH_END();

    // Rebuild callback-backed superglobals now; a false return clears armed.
    ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), auto_global) {
        if (auto_global->name == _env) {
            continue;
        }
        if (auto_global->auto_global_callback) {
            auto_global->armed =
                auto_global->auto_global_callback(auto_global->name);
        }
    }
    ZEND_HASH_FOREACH_END();
}
#ifdef HAVE_PHP_SESSION
// A bailout of a save handler must reach the catch of the caller, so this
// function has no zend_try.
static void rapira_reset_session(void) {
    if (PS(session_status) == php_session_active) {
        php_session_flush(1); // write and close the active session
    }
    if (!Z_ISUNDEF(PS(http_session_vars))) {
        zval_ptr_dtor(&PS(http_session_vars));
        ZVAL_UNDEF(&PS(http_session_vars));
    }
    if (PS(mod_data) || PS(mod_user_implemented)) {
        PS(mod)->s_close(&PS(mod_data));
    }
    if (PS(id)) {
        zend_string_release_ex(PS(id), false);
        PS(id) = NULL;
    }
    if (PS(session_vars)) {
        zend_string_release_ex(PS(session_vars), false);
        PS(session_vars) = NULL;
    }
    if (PS(session_started_filename)) {
        zend_string_release(PS(session_started_filename));
        PS(session_started_filename) = NULL;
        PS(session_started_lineno) = 0;
    }
    PS(session_status) = php_session_none;
    // php_rinit_session_globals() resets these per request, and the worker path
    // skips RINIT
    PS(mod_user_is_open) = false;
    PS(in_save_handler) = false;
    PS(set_handler) = false;
    // a cookie-sourced id clears it (session.c:1564) and only RINIT restores it
    PS(define_sid) = true;
}
#else
static void rapira_reset_session(void) {}
#endif

static void rapira_reset_super_global(void) {
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    zval_ptr_dtor(files);
    ZVAL_UNDEF(files);
    // $_SESSION may be IS_INDIRECT; only _del_ind follows the indirection
    zend_hash_str_del_ind(&EG(symbol_table), "_SESSION",
                          sizeof("_SESSION") - 1);
}
// A pending throw is uncaught. With call_handler, the user exception handler
// gets it first. Without call_handler, or when the throw stays pending, it
// becomes the fatal record. Returns true for the record. In PHP 8.4 and later,
// exit() and die() are not bailouts. They land in EG(exception), and this
// function clears them.
static bool rapira_report_uncaught(bool call_handler) {
    if (!EG(exception)) {
        return false;
    }
    if (zend_is_unwind_exit(EG(exception)) ||
        zend_is_graceful_exit(EG(exception))) {
        zend_clear_exception();
        return false;
    }
    if (call_handler) {
        zend_try_exception_handler();
        if (!EG(exception)) {
            return false;
        }
    }
    // the Throwable path uses E_DONT_BAIL and releases the object
    zend_exception_error(EG(exception), E_ERROR);
    return true;
}

int rapira_run_handler(zend_fcall_info *fci, zend_fcall_info_cache *fcc) {
    int outcome = RAPIRA_OK;
    zval retval;
    ZVAL_UNDEF(&retval);
    // fci does not outlive this frame
    // cppcheck-suppress autoVariables
    fci->retval = &retval;

    zend_execute_data *observed_base = EG(current_observed_frame);
    rapira_in_handler = true;
    RAPIRA_GUARD(
        {
            zend_call_function(fci, fcc);
            zval_ptr_dtor(&retval);
        },
        outcome, observed_base);

    // the guard catches a bailout from the user exception handler
    RAPIRA_GUARD(
        if (rapira_report_uncaught(true)) { outcome = RAPIRA_THROW; }, outcome,
        observed_base);

    // a destructor sweep per unit would run __destruct on live objects
    RAPIRA_OBSERVER_CLOSE(php_call_shutdown_functions(), observed_base);
    // the internal frame of handle_request is live, so zend_call_function
    // leaves a throw of a shutdown function pending (Zend/zend_execute_API.c)
    RAPIRA_GUARD(
        if (rapira_report_uncaught(true)) { outcome = RAPIRA_THROW; }, outcome,
        observed_base);
    // the table release frees closure captures, which can run __destruct
    RAPIRA_OBSERVER_CLOSE(php_free_shutdown_functions(), observed_base);

    // this must follow every step that runs app code, or the exception is
    // thrown again later. The release can run a __destruct that bails out
    RAPIRA_GUARD(
        if (EG(exception)) { zend_clear_exception(); }, outcome, observed_base);

    gc_protect(false); // _zend_bailout can leave it engaged
    rapira_in_handler = false;

    // only _zend_bailout sets it mid-request (zend.c:1264), and
    // rapira_request_init clears it per unit
    if (outcome != RAPIRA_BAILOUT && CG(unclean_shutdown)) {
        outcome = RAPIRA_BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    return outcome;
}

int rapira_request_activate(void) {
    int outcome = RAPIRA_OK;
    zend_try {
        php_output_activate();
        sapi_activate();
        rapira_modules_request(true);
        rapira_request_init();
        rapira_reset_super_global();
        rapira_activate_auto_globals();
    }
    zend_catch { outcome = RAPIRA_BAILOUT; }
    zend_end_try();

    if (outcome == RAPIRA_BAILOUT) {
        gc_protect(false);
    }

    return outcome;
}

// sapi_activate resets the slot with no release (SAPI.c), so it leaks per unit
static void rapira_release_header_callback(void) {
#if PHP_VERSION_ID >= 80600
    if (ZEND_FCC_INITIALIZED(SG(send_header_fcc))) {
        zend_fcc_dtor(
            &SG(send_header_fcc)); // self-resets to empty_fcall_info_cache
    }
#else
    if (!Z_ISUNDEF(SG(callback_func))) {
        zval_ptr_dtor(&SG(callback_func));
        ZVAL_UNDEF(&SG(callback_func));
    }
#endif
}

// the per-request SAPI teardown (main/main.c:1985,2002,2031)
int rapira_request_teardown(void) {
    int outcome = RAPIRA_OK;
    // the VM pops the stack when handle_request returns, so close frames here
    zend_execute_data *observed_base = EG(current_observed_frame);

    // the output is complete, so publish the response before the module and
    // SAPI teardown. One try covers both calls. After a flush bailout the head
    // stays unsent, and the seal commits the 500 head.
    RAPIRA_GUARD(
        {
            php_output_end_all();
            php_header();
        },
        outcome, observed_base);
    // the session write comes before the response, because a store without a
    // lock serves the old session to a request that starts when the response
    // arrives
    RAPIRA_GUARD(rapira_reset_session(), outcome, observed_base);
    rapira_rs_finish_response();
    // the internal frame of handle_request is live, so a throw of the
    // teardown flush or of the save handler stays pending. It gets only the
    // fatal record. The record comes after the seal, so it does not cut a
    // streamed body. The user exception handler does not run, because it
    // could start a session after the session reset.
    RAPIRA_GUARD(
        if (rapira_report_uncaught(false) && outcome == RAPIRA_OK) {
            outcome = RAPIRA_THROW;
        },
        outcome, observed_base);
    RAPIRA_GUARD(rapira_modules_request(false), outcome, observed_base);
    RAPIRA_GUARD(php_output_deactivate(), outcome, observed_base);
    RAPIRA_GUARD(rapira_release_header_callback(), outcome, observed_base);
    RAPIRA_GUARD(sapi_deactivate(), outcome, observed_base);

    // The next unit re-arms the timer in rapira_request_init(), and next_unit()
    // disarms it before parking or stopping.

    // the teardown can run __destruct, and a pending throw would reach the next
    // unit. The release can run a __destruct that bails out.
    RAPIRA_GUARD(
        if (EG(exception)) { zend_clear_exception(); }, outcome, observed_base);

    // _zend_bailout leaves gc_protect engaged, so GC is off in the next unit
    gc_protect(false);

    // unbind_server_context (context.rs) clears the other request_info pointers
    // after each unit. sapi_deactivate_module frees both and keeps the pointers
    // (main/SAPI.c). The sapi_deactivate at cycle end would free them again
    SG(request_info).current_user = NULL;
    SG(request_info).content_type_dup = NULL;

    return outcome;
}

// A last error that stays set keeps objects alive and fails the assertion in
// core_globals_dtor (main.c:2102).
void rapira_clear_last_error(void) {
    if (PG(last_error_message)) {
        PG(last_error_type) = 0;
        PG(last_error_lineno) = 0;
        zend_string_release(PG(last_error_message));
        PG(last_error_message) = NULL;

        if (PG(last_error_file)) {
            zend_string_release(PG(last_error_file));
            PG(last_error_file) = NULL;
        }
    }
#if PHP_VERSION_ID >= 80500
    // the captured trace keeps request objects alive, and only
    // shutdown_executor frees it. The dtor is valid only inside a live request.
    // After php_request_shutdown, rapira_request_shutdown clears the stale zval
    // without a dtor
    zend_try {
        zval_ptr_dtor(&EG(last_fatal_error_backtrace));
        ZVAL_UNDEF(&EG(last_fatal_error_backtrace));
    }
    zend_catch { ZVAL_UNDEF(&EG(last_fatal_error_backtrace)); }
    zend_end_try();
#endif
}

bool rapira_zts(void) { return PHP_ZTS; }

bool rapira_has_execution_timers(void) {
#ifdef ZEND_MAX_EXECUTION_TIMERS
    return true;
#else
    return false;
#endif
}

// MINIT still owns the ini table that each worker copies.
// https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_ini.c#L144-L171
void rapira_lock_timeouts(void) {
    if (rapira_timeouts_disabled) {
        // set_time_limit() uses PHP_INI_USER to change max_execution_time.
        // https://github.com/php/php-src/blob/php-8.6.0/main/main.c#L1603-L1616
        // ini_set() also uses PHP_INI_USER.
        // https://github.com/php/php-src/blob/php-8.6.0/ext/standard/basic_functions.c#L2008-L2054
        zend_ini_entry *execution = zend_hash_str_find_ptr(
            EG(ini_directives), ZEND_STRL("max_execution_time"));
        zend_ini_entry *input = zend_hash_str_find_ptr(
            EG(ini_directives), ZEND_STRL("max_input_time"));
        if (execution) {
            execution->modifiable = PHP_INI_SYSTEM;
        }
        if (input) {
            input->modifiable = PHP_INI_SYSTEM;
        }
    }
}

// Start TSRM before Zend signal handling and SAPI startup.
// https://github.com/php/php-src/blob/php-8.6.0/sapi/embed/php_embed.c#L167-L186
bool rapira_process_init(int expected_threads, bool disable_timeouts) {
    rapira_timeouts_disabled = disable_timeouts;
#ifdef ZTS
    if (!php_tsrm_startup_ex(expected_threads)) {
        return false;
    }
#else
    (void)expected_threads;
#endif
    zend_signal_startup();
    return true;
}

// Only the thread that started TSRM can shut it down.
// https://github.com/php/php-src/blob/php-8.6.0/TSRM/TSRM.c#L194-L206
void rapira_process_shutdown(void) {
#if !defined(ZTS) && defined(ZEND_CHECK_STACK_LIMIT)
    // The one NTS interpreter returns to the main stack for module shutdown.
    zend_call_stack_init();
#endif
    php_module_shutdown();
    sapi_shutdown();
#ifdef ZTS
    tsrm_shutdown();
#endif
}

// Returns NULL when opcache.restrict_api is set. php-src refuses the API with a
// warning when path_translated is not under restrict_api, and the host calls
// run outside a unit, where path_translated is NULL
// (zend_accelerator_module.c:65). The lookup runs at each call, because on PHP
// 8.4 opcache.so can load after this module.
static zend_function *rapira_opcache_function(const char *name) {
    const char *restrict_api =
        zend_ini_string(ZEND_STRL("opcache.restrict_api"), 0);
    if (restrict_api != NULL && *restrict_api != '\0') {
        return NULL;
    }
    return zend_hash_str_find_ptr(CG(function_table), name, strlen(name));
}

// The OPcache state of this process as flags. Value 1: OPcache serves this app
// instance. Value 2: a restart is pending. Value 4: a restart runs now. -1: no
// OPcache, opcache.file_cache_only, opcache.restrict_api or a memory_limit
// error in this request. *last_restart gets last_restart_time, which a restart
// changes after it turns OPcache on again
// (ZEND_RINIT_FUNCTION(zend_accelerator)). opcache_get_status() is the only
// exported reader. It needs an active request.
int rapira_opcache_state(zend_long *last_restart) {
#ifdef ZEND_HAS_OOM
    // after a memory_limit error, php-src calls no function until the request
    // shutdown. zend_call_function() fails, and a debug build can assert
    // https://github.com/php/php-src/blob/8eb02dbaa339613075d1c9db9f130eb7e4c8af73/Zend/zend_execute_API.c#L849-L853
    if (ZEND_HAS_OOM()) {
        return -1;
    }
#endif
    zend_function *fn = rapira_opcache_function("opcache_get_status");
    if (fn == NULL) {
        return -1;
    }
    zval scripts, status;
    ZVAL_FALSE(&scripts);
    zend_fcall_info fci = {
        .size = sizeof(fci),
        .retval = &status,
        .params = &scripts,
        .param_count = 1,
    };
    ZVAL_UNDEF(&fci.function_name);
    zend_fcall_info_cache fcc = {.function_handler = fn};
    int state = -1;
    // the call allocates, and a memory_limit bailout with no jump target is
    // exit(-1) (zend.c:1265). From php-8.4.27 and php-8.5.12, every call fails
    // after a memory_limit fatal error (zend_execute_API.c:838), and
    // zend_call_known_function then raises E_CORE_ERROR.
    zend_try {
        // with file_cache_only the array has no restart keys
        // (zend_accelerator_module.c:705)
        if (zend_call_function(&fci, &fcc) == SUCCESS &&
            Z_TYPE(status) == IS_ARRAY &&
            !zend_hash_str_exists(Z_ARRVAL(status),
                                  ZEND_STRL("file_cache_only"))) {
            HashTable *ht = Z_ARRVAL(status);
            zval *on = zend_hash_str_find(ht, ZEND_STRL("opcache_enabled"));
            zval *pending =
                zend_hash_str_find(ht, ZEND_STRL("restart_pending"));
            zval *running =
                zend_hash_str_find(ht, ZEND_STRL("restart_in_progress"));
            zval *stats =
                zend_hash_str_find(ht, ZEND_STRL("opcache_statistics"));
            zval *last =
                stats != NULL && Z_TYPE_P(stats) == IS_ARRAY
                    ? zend_hash_str_find(Z_ARRVAL_P(stats),
                                         ZEND_STRL("last_restart_time"))
                    : NULL;
            state = (on != NULL && Z_TYPE_P(on) == IS_TRUE ? 1 : 0) |
                    (pending != NULL && Z_TYPE_P(pending) == IS_TRUE ? 2 : 0) |
                    (running != NULL && Z_TYPE_P(running) == IS_TRUE ? 4 : 0);
            *last_restart =
                last != NULL && Z_TYPE_P(last) == IS_LONG ? Z_LVAL_P(last) : 0;
        }
        zval_ptr_dtor(&status);
    }
    zend_catch { return -1; }
    zend_end_try();
    return state;
}

// Calls opcache_reset(). It does nothing without OPcache or with
// opcache.restrict_api. It needs an active request. The zend_try has the same
// reason as the one in rapira_opcache_state.
void rapira_opcache_reset(void) {
    zend_function *fn = rapira_opcache_function("opcache_reset");
    if (fn == NULL) {
        return;
    }
    zval retval;
    ZVAL_UNDEF(&retval);
    zend_fcall_info fci = {
        .size = sizeof(fci),
        .retval = &retval,
    };
    ZVAL_UNDEF(&fci.function_name);
    zend_fcall_info_cache fcc = {.function_handler = fn};
    zend_try {
        zend_call_function(&fci, &fcc);
        zval_ptr_dtor(&retval);
    }
    zend_end_try();
}

// Zero-copy read-only body stream backed by Rust Bytes chunks (callbacks.rs).
extern void *rapira_rs_body_open(void);
extern void *rapira_rs_body_clone(const void *cursor);
extern size_t rapira_rs_body_read(void *cursor, char *buf, size_t count,
                                  bool *eof);
extern int rapira_rs_body_seek(void *cursor, int64_t offset, int whence,
                               int64_t *newoffs);
extern size_t rapira_rs_body_len(const void *cursor);
extern void rapira_rs_body_drop(void *cursor);
// The Authorization field of the bound request (callbacks.rs); NULL when none.
extern const char *rapira_rs_authorization(void);

static ssize_t rapira_body_stream_write(php_stream *stream, const char *buf,
                                        size_t count) {
    (void)stream;
    (void)buf;
    (void)count;
    return -1;
}

static ssize_t rapira_body_stream_read(php_stream *stream, char *buf,
                                       size_t count) {
    bool eof = false;
    size_t n = rapira_rs_body_read(stream->abstract, buf, count, &eof);
    if (eof) {
        stream->eof = 1;
    }
    return (ssize_t)n;
}

static int rapira_body_stream_close(php_stream *stream, int close_handle) {
    (void)close_handle;
    if (stream->abstract != NULL) {
        rapira_rs_body_drop(stream->abstract);
        stream->abstract = NULL;
    }
    return 0;
}

static int rapira_body_stream_flush(php_stream *stream) {
    (void)stream;
    return 0;
}

static int rapira_body_stream_seek(php_stream *stream, zend_off_t offset,
                                   int whence, zend_off_t *newoffs) {
    int64_t pos = -1;
    if (rapira_rs_body_seek(stream->abstract, (int64_t)offset, whence, &pos) !=
        0) {
        *newoffs = -1;
        return -1;
    }
    *newoffs = (zend_off_t)pos;
    stream->eof = 0;
    stream->fatal_error = 0;
    return 0;
}

static int rapira_body_stream_stat(php_stream *stream,
                                   php_stream_statbuf *ssb) {
    memset(ssb, 0, sizeof(php_stream_statbuf));
    ssb->sb.st_mode = 0444 | S_IFREG;
    ssb->sb.st_size = (zend_off_t)rapira_rs_body_len(stream->abstract);
    ssb->sb.st_nlink = 1;
    ssb->sb.st_rdev = -1;
    ssb->sb.st_dev = 0xC;
    ssb->sb.st_ino = 0;
#ifndef PHP_WIN32
    ssb->sb.st_blksize = -1;
    ssb->sb.st_blocks = -1;
#endif
    return 0;
}

static const php_stream_ops rapira_body_stream_ops = {
    rapira_body_stream_write,
    rapira_body_stream_read,
    rapira_body_stream_close,
    rapira_body_stream_flush,
    "MEMORY",
    rapira_body_stream_seek,
    NULL, /* cast */
    rapira_body_stream_stat,
    NULL, /* set_option */
};

static const php_stream_ops rapira_input_stream_ops = {
    rapira_body_stream_write,
    rapira_body_stream_read,
    rapira_body_stream_close,
    rapira_body_stream_flush,
    "Input",
    rapira_body_stream_seek,
    NULL, /* cast */
    rapira_body_stream_stat,
    NULL, /* set_option */
};

static const php_stream_wrapper *rapira_orig_php_wrapper = NULL;

// Serves php://input directly over the zero-copy body chunks with stat()
// support so file_get_contents('php://input') allocates the full buffer once
// and avoids repeated 8 KiB reallocations on multi-megabyte bodies. Every other
// php:// path delegates to stock PHP.
static php_stream *
rapira_php_stream_opener(php_stream_wrapper *wrapper, const char *path,
                         const char *mode, int options,
                         zend_string **opened_path,
                         php_stream_context *context STREAMS_DC) {
    const char *sub = path;
    if (!strncasecmp(sub, "php://", 6)) {
        sub += 6;
    }
    if (!strcasecmp(sub, "input") && SG(request_info).request_body != NULL &&
        SG(request_info).request_body->ops == &rapira_body_stream_ops) {
        if ((options & STREAM_OPEN_FOR_INCLUDE) && !PG(allow_url_include)) {
            if (options & REPORT_ERRORS) {
                php_error_docref(
                    NULL, E_WARNING,
                    "URL file-access is disabled in the server configuration");
            }
            return NULL;
        }
        void *cursor =
            rapira_rs_body_clone(SG(request_info).request_body->abstract);
        if (cursor != NULL) {
            php_stream *stream =
                php_stream_alloc(&rapira_input_stream_ops, cursor, 0, "rb");
            stream->flags |= PHP_STREAM_FLAG_NO_BUFFER;
            return stream;
        }
    }
    return rapira_orig_php_wrapper->wops->stream_opener(
        wrapper, path, mode, options, opened_path, context STREAMS_REL_CC);
}

static const php_stream_wrapper_ops rapira_php_wops = {
    rapira_php_stream_opener,
    NULL, /* close */
    NULL, /* fstat */
    NULL, /* stat */
    NULL, /* opendir */
    "PHP",
    NULL, /* unlink */
    NULL, /* rename */
    NULL, /* mkdir */
    NULL, /* rmdir */
    NULL,
};

static const php_stream_wrapper rapira_php_wrapper = {
    &rapira_php_wops,
    NULL,
    0, /* is_url */
};

// sapi_deactivate_module only sets the request body stream to NULL, and no code
// frees the resource. The body is a rapira zero-copy stream (or memory/temp
// stream from fallback paths).
void rapira_release_temporary_streams(void) {
    zend_resource *val = NULL;
    int stream_type = php_file_le_stream();
    ZEND_HASH_FOREACH_PTR(&EG(regular_list), val) {
        if (val->type == stream_type) {
            php_stream *stream = val->ptr;
            if (stream != NULL &&
                (stream->ops == &rapira_body_stream_ops ||
                 stream->ops == &php_stream_memory_ops ||
                 stream->ops == &php_stream_temp_ops) &&
                !(stream->flags & PHP_STREAM_FLAG_NO_FCLOSE) &&
                stream->__exposed == 0 && GC_REFCOUNT(val) == 1) {
                zend_list_delete(val);
            }
        }
    }
    ZEND_HASH_FOREACH_END();
}

// Hands the body to PHP as a zero-copy read-only stream over the request
// chunks; rapira_release_temporary_streams frees the stream between units. With
// post_read set, php://input does not call read_post.
static void rapira_install_body(void) {
    void *cursor = rapira_rs_body_open();
    if (cursor == NULL) {
        return;
    }
    php_stream *stream =
        php_stream_alloc(&rapira_body_stream_ops, cursor, 0, "rb");
    stream->flags |= PHP_STREAM_FLAG_NO_BUFFER;
    SG(request_info).request_body = stream;
    SG(post_read) = 1;
}

// The post reader of the urlencoded entry: the post_max_size check of
// sapi_read_standard_form_data (main/SAPI.c), then the body.
static void rapira_post_reader(void) {
    zend_long post_max_size =
        REQUEST_PARSE_BODY_OPTION_GET(post_max_size, SG(post_max_size));
    if (post_max_size > 0 && SG(request_info).content_length > post_max_size) {
        php_error_docref(NULL, E_WARNING,
                         "POST Content-Length of " ZEND_LONG_FMT
                         " bytes exceeds the limit of " ZEND_LONG_FMT " bytes",
                         SG(request_info).content_length, post_max_size);
        return;
    }
    rapira_install_body();
}

// Like php_default_post_reader (main/php_content_types.c), reads the body only
// for a POST with no entry.
static void rapira_default_post_reader(void) {
    if (!strcmp(SG(request_info).request_method, "POST") &&
        !SG(request_info).post_entry) {
        rapira_post_reader();
    }
}

// Every other request with a body gets the body here: PUT, PATCH, and a POST
// without a content type. A multipart POST has an entry, and rfc1867 reads it
// through read_post.
static int rapira_activate(void) {
    if (!SG(request_info).request_body && !SG(request_info).post_entry) {
        rapira_install_body();
    }
    // php_handle_auth_data allocates. A memory_limit bailout must land in the
    // zend_try around sapi_activate and must not cross a Rust frame
    php_handle_auth_data(rapira_rs_authorization());
    return SUCCESS;
}

// Register after PHP installs its default POST reader at startup.
// https://github.com/php/php-src/blob/php-8.6.0/main/php_content_types.c#L39-L46
void rapira_register_post_reader(void) {
    sapi_register_default_post_reader(rapira_default_post_reader);
    sapi_module.activate = rapira_activate;
    php_stream_wrapper *orig = zend_hash_str_find_ptr(
        php_stream_get_url_stream_wrappers_hash_global(), "php", 3);
    if (orig != NULL && orig->wops != NULL &&
        orig->wops->stream_opener != NULL) {
        rapira_orig_php_wrapper = orig;
        php_unregister_url_stream_wrapper("php");
        php_register_url_stream_wrapper("php", &rapira_php_wrapper);
    }
}

// The ZTS thread constructor copies the ini entries and initializes the stack
// limit and the timer.
// https://github.com/php/php-src/blob/php-8.6.0/Zend/zend.c#L883-L891
void rapira_thread_start(int mode, const rapira_plugin *plugin,
                         const char *name, size_t name_len, bool cpu_clock) {
#ifdef ZTS
    (void)ts_resource(0);
#elif defined(ZEND_CHECK_STACK_LIMIT)
    // On NTS, MINIT set the limit for the main thread.
    zend_call_stack_init();
#endif
    rapira_set_dispatcher(mode, plugin, name, name_len);
    rapira_cpu_clock = cpu_clock;
    // Each thread has its own content type table. Keep the POST handler that
    // mbstring selected.
    // https://github.com/php/php-src/blob/php-8.6.0/main/SAPI.c#L50-L55
    sapi_post_entry *entry = zend_hash_str_find_ptr(
        &SG(known_post_content_types), DEFAULT_POST_CONTENT_TYPE,
        sizeof(DEFAULT_POST_CONTENT_TYPE) - 1);
    if (entry) {
        entry->post_reader = rapira_post_reader;
    }
}

void rapira_thread_end(void) {
#ifdef ZTS
#if PHP_VERSION_ID < 80700
    // PHP 8.4 to 8.6 destroy only the lists still in TSRM at module shutdown.
    // PHP 8.7 destroys the list in ts_free_thread().
    // https://github.com/php/php-src/commit/0a98b97933070124575e12c0bbdfd18371e5def9
    dtor_func_t destructor = EG(persistent_list).pDestructor;
    volatile bool completed = false;
    do {
        zend_try {
            zend_hash_graceful_reverse_destroy(&EG(persistent_list));
            completed = true;
        }
        zend_end_try();
    } while (!completed);
    zend_hash_init(&EG(persistent_list), 8, NULL, destructor, true);
#endif
    ts_free_thread();
#endif
}

// Boot-registered shutdown functions run once, at cycle end.
ZEND_TLS HashTable *rapira_boot_shutdown_functions = NULL;

void rapira_stash_boot_shutdown_functions(void) {
    rapira_boot_shutdown_functions = BG(user_shutdown_function_names);
    BG(user_shutdown_function_names) = NULL;
}

static void rapira_restore_boot_shutdown_functions(void) {
    HashTable *boot = rapira_boot_shutdown_functions;
    if (!boot) {
        return;
    }
    rapira_boot_shutdown_functions = NULL;

    HashTable *late = BG(user_shutdown_function_names);
    if (late) {
        zend_string *key = NULL;
        zval *entry = NULL;
        ZEND_HASH_FOREACH_STR_KEY_VAL(late, key, entry) {
            // only register_user_shutdown_function() uses a name as the key
            // (ext/session). The PHP function register_shutdown_function()
            // appends with a numeric key
            if (key) {
                zend_hash_update(boot, key, entry);
            } else {
                zend_hash_next_index_insert(boot, entry);
            }
        }
        ZEND_HASH_FOREACH_END();
        late->pDestructor = NULL;
        php_free_shutdown_functions();
    }
    BG(user_shutdown_function_names) = boot;
}

// the retry is safe, because end_all sets EG(current_observed_frame) to NULL
// (zend_observer.c:322)
int rapira_request_shutdown(void) {
    volatile int bailed = RAPIRA_OK;
    // arm the budget again, because a stale 0 disables max_execution_time in
    // the next cycle, and the boot shutdown functions run under the timer until
    // php_request_shutdown disarms it (main/main.c:1993)
    if (rapira_unit_timeout >= 0) {
        zend_set_timeout(rapira_unit_timeout, false);
    }
    rapira_unit_timeout = -1; // the next cycle captures its budget again
    rapira_timer_armed = false;
    rapira_timer_armed_ms = 0;
#ifdef HAVE_PHP_SESSION
    // left set, it makes RSHUTDOWN skip the handler's close() (mod_user.c:29)
    PS(in_save_handler) = false;
#endif
    // the restore is inside the try, because its hash inserts allocate, and a
    // bailout with no jump target set is exit(-1) (zend.c:1258)
    zend_try {
        rapira_restore_boot_shutdown_functions();
        php_request_shutdown(NULL);
    }
    zend_catch {
        bailed = RAPIRA_BAILOUT;
        zend_try { php_request_shutdown(NULL); }
        zend_end_try();
    }
    zend_end_try();
    // The putenv() writes end with the PHP request, after every RSHUTDOWN.
    rapira_rs_env_clear();
    rapira_dispatcher_forget();
#if PHP_VERSION_ID >= 80500
    // fast shutdown skips the backtrace release (zend_execute_API.c:282,309)
    // and the arena free reclaimed it; drop the stale zval without a dtor
    ZVAL_UNDEF(&EG(last_fatal_error_backtrace));
#endif
    // sapi_deactivate_module frees content_type_dup and keeps the pointer, and
    // sapi_activate clears it only for a request with a server context
    // (main/SAPI.c). The next request can be one with no client
    SG(request_info).content_type_dup = NULL;
    return bailed;
}
