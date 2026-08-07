#include "php.h"
#include "rapira_classes.h"
#include "wrapper.h"
#include "zend_types.h"

extern void rapira_rs_finish_response(void); // Rust: ctx.finish()

// ub_write's client-abort raise (php_handle_aborted_connection,
// main/main.c:2722) longjmps, so run it here in C: the Rust half reports the
// disconnect via *aborted, and this raises the abort AFTER the Rust
// catch_unwind frame has returned.
extern size_t rapira_rs_ub_write(const char *str, size_t len, bool *aborted);
size_t rapira_ub_write(const char *str, size_t len) {
    bool aborted = false;
    size_t written = rapira_rs_ub_write(str, len, &aborted);
    if (aborted) {
        php_handle_aborted_connection();
    }
    return written;
}

// Keep in sync with Outcome in types.rs (#[repr(C)]).
enum {
    OK = 0,
    BAILOUT = 1,
    EXIT = 2,
    THROW = 3,
};

// Guard a teardown step: on bailout, flag it and close the observer frames the
// longjmp abandoned.
#define RAPIRA_GUARD(stmt, flag, base)                                         \
    zend_try { stmt; }                                                         \
    zend_catch {                                                               \
        (flag) = BAILOUT;                                                      \
        rapira_observer_end_to(base);                                          \
    }                                                                          \
    zend_end_try()

// Like RAPIRA_GUARD, but for a step whose bailout records no outcome - just
// close the observer frames the longjmp abandoned and continue teardown.
#define RAPIRA_OBSERVER_CLOSE(stmt, base)                                      \
    zend_try { stmt; }                                                         \
    zend_catch { rapira_observer_end_to(base); }                               \
    zend_end_try()

// php_output_end_all flushes userland output handlers, which can fatal and
// bailout; guard it and report BAILOUT.
int rapira_finish_output(void) {
    zend_try {
        php_output_end_all();
        php_header(); // no-op if SG(headers_sent) is true
    }
    zend_catch { return BAILOUT; }
    zend_end_try();

    return OK;
}

PHP_FUNCTION(rapira_finish_request) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (rapira_finish_output()) { // non-zero == BAILOUT: re-raise so
        zend_bailout(); // rapira_run_handler classifies + 500s + recycles
    }
    rapira_rs_finish_response();
    RETURN_TRUE;
}

PHP_MINIT_FUNCTION(rapira) {
    rapira_register_classes();
    return SUCCESS;
}

PHP_RSHUTDOWN_FUNCTION(rapira) {
	rapira_dispatcher_release();
	return SUCCESS;
}

zend_module_entry rapira_module_entry = {
    STANDARD_MODULE_HEADER,
    "rapira",
    NULL, // functions: installed by rapira_process_init
    PHP_MINIT(rapira),
    NULL,
    NULL,
    PHP_RSHUTDOWN(rapira),
    NULL, // MSHUTDOWN, RINIT, RSHUTDOWN, MINFO
    "0.1.0",
    STANDARD_MODULE_PROPERTIES};

// ext/filter caches raw input copies in its module globals; only its RSHUTDOWN
// frees them (filter.c:190-196), and sapi_activate()'s input_filter_init()
// UNDEFs them WITHOUT freeing (filter.c:232-240, wired via SAPI.c:484). The
// worker path runs sapi_activate per job but RSHUTDOWN only at cycle end, so
// each job would orphan the previous job's arrays - run the module's own
// request cycle per job.
static const char *RELOAD_MODULES[] = {"filter", NULL};

// Run the per-request startup (startup=true) or shutdown hook of each
// RELOAD_MODULE.
static void rapira_modules_request(bool startup) {
    zend_module_entry *module = NULL;
    for (const char **name = RELOAD_MODULES; *name; name++) {
        module = zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
        if (!module) {
            continue;
        }
        if (startup && module->request_startup_func) {
            // engine contract: RINIT failure is fatal (zend_activate_modules
            // warns and exit(1)s). Bail instead - the zend_try in
            // rapira_request_activate contains it, failing the job and
            // recycling the worker.
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

// A bailout longjmps past the observer end handlers of every frame it abandons.
// php_request_shutdown repairs this via zend_observer_fcall_end_all(), which
// worker mode never reaches. Close only frames above `base` - the resident
// script's own observed frames must stay open, so plain end_all() would be
// wrong here.
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

// Per-job execution budget, captured once per cycle. EG(timeout_seconds) is
// runtime-mutable (set_time_limit()/ini_set() go through OnUpdateTimeout) and
// the worker path never restores INI per job, so re-arming straight from it
// would let one job's set_time_limit(0) disable the timer for every later job.
// The first rapira_request_init of a cycle runs after the bootstrap top-level,
// so the capture honors the bootstrap's own set_time_limit;
// rapira_request_shutdown resets it at cycle end. -1 = not captured yet.
static zend_long rapira_job_timeout = -1;

// Per-request state php_request_startup() resets that the worker path skips.
static void rapira_request_init(void) {
    PG(connection_status) = PHP_CONNECTION_NORMAL;
    PG(header_is_being_sent) = 0;
    // recursion/context guards a bailout can strand: a fatal inside a userland
    // include-wrapper's stream_open leaves in_user_include=1 (later URL opens
    // rejected as includes); a fatal inside php_log_err leaves in_error_log=1
    // (later error-log writes dropped).
    PG(in_error_log) = 0;
    PG(in_user_include) = 0;
    // php-src clears this per request via init_compiler (zend_compile.c:461); a
    // resident worker runs many jobs per cycle, and a non-recycling client
    // abort leaves it =1 (zend.c:1264), defeating rapira_run_handler's
    // clean_at_entry detector for later jobs. Re-clear it at each job start.
    CG(unclean_shutdown) = 0;

    // The request body is read (in Rust) as the handler runs, so the handler is
    // the execution phase: bound it by max_execution_time (the per-cycle
    // capture above), not max_input_time. Teardown unsets it, so time spent
    // blocked on the job queue is never counted.
    //
    // reset_signals=0: with reset_signals, zend_set_timeout reinstalls the
    // SIGRTMIN handler + unblocks it on every call (an rt_sigaction +
    // rt_sigprocmask syscall per job). Both are already in place for this
    // thread: php_request_startup runs zend_set_timeout(..., 1) at cycle boot,
    // the disposition is process-wide, and nothing re-blocks SIGRTMIN between
    // jobs (rapira only blocks INT/TERM). Same per-request re-arm as
    // php_execute_script (main.c:2630).
    // https://man7.org/linux/man-pages/man7/signal.7.html
    // https://man7.org/linux/man-pages/man2/sigaction.2.html
    // https://man7.org/linux/man-pages/man2/sigprocmask.2.html
    if (rapira_job_timeout < 0) {
        rapira_job_timeout = EG(timeout_seconds);
    }
    zend_set_timeout(rapira_job_timeout, 0);

    if (PG(expose_php)) {
        sapi_add_header(SAPI_PHP_VERSION_HEADER,
                        sizeof(SAPI_PHP_VERSION_HEADER) - 1, 1);
    }

    // main/main.c php_request_startup(): honor the output INIs per request.
    // 8.6 stores the output_handler ini as zend_string* via
    // OnUpdateStrNotEmpty, so an empty value is NULL and the [0] emptiness
    // check is gone:
    // https://github.com/php/php-src/commit/e0221be81e39860e83867fadd67115e1d2c992c1
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

// Superglobals are registered as zend_auto_globals in CG(auto_globals) (struct:
// Zend/zend_compile.h). sapi_activate would re-arm + rebuild them per request;
// worker mode skips it, so do it here.
static void rapira_activate_auto_globals(void) {
    zend_auto_global *auto_global = NULL;
    zend_string *_env = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_ENV);

    // Re-arm every superglobal so a later access rebuilds it.
    ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), auto_global) {
        auto_global->armed =
            ((auto_global->jit || auto_global->auto_global_callback) != 0);
    }
    ZEND_HASH_FOREACH_END();

    // Eagerly rebuild every callback-backed superglobal now (a create callback
    // that returns false clears armed). $_ENV is skipped: the environment is
    // constant and reset_super_globals() keeps $_ENV in the symbol table, so
    // its first-use array stays correct without a per-request rebuild.
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
// Worker mode bypasses module RSHUTDOWN, so the session module never flushes/
// closes an active session between requests - do the work session_rshutdown
// would have done, or PS(id)/session_status leak across requests (one request
// reusing the previous one's session).
//
// Nothing here is guarded on purpose. A bailing save handler is a fatal: it
// must reach rapira_request_teardown's zend_catch and recycle the worker, so
// php_request_shutdown runs the module's own RSHUTDOWN and closes the observer
// frames the longjmp abandoned. Swallowing it here and then calling more PHP
// (s_close) over those frames is what corrupts EG(current_observed_frame).
static void rapira_reset_session(void) {
    if (PS(session_status) == php_session_active) {
        php_session_flush(1); // write + close the active session
    }
    if (!Z_ISUNDEF(PS(http_session_vars))) {
        zval_ptr_dtor(&PS(http_session_vars));
        ZVAL_UNDEF(&PS(http_session_vars));
    }
    if (PS(mod_data) || PS(mod_user_implemented)) {
        PS(mod)->s_close(&PS(mod_data));
    }
    if (PS(id)) {
        zend_string_release_ex(PS(id), 0);
        PS(id) = NULL;
    }
    if (PS(session_vars)) {
        zend_string_release_ex(PS(session_vars), 0);
        PS(session_vars) = NULL;
    }
    if (PS(session_started_filename)) {
        zend_string_release(PS(session_started_filename));
        PS(session_started_filename) = NULL;
        PS(session_started_lineno) = 0;
    }
    PS(session_status) = php_session_none;
    // Transient guards that php_rinit_session_globals() scrubs at each request
    // start. Worker mode skips RINIT, so a userland save handler that bails
    // mid-call (or uses partial parent:: delegation) can leave them set. Cheap
    // defensive reset:
    PS(mod_user_is_open) = 0;
    PS(in_save_handler) = 0;
    PS(set_handler) = 0;
}
#else
static void rapira_reset_session(void) {}
#endif

static void rapira_reset_super_global(void) {
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    zval_ptr_dtor(files);
    ZVAL_UNDEF(files);
    // A top-level $_SESSION is an IS_INDIRECT symbol-table entry aliasing the
    // main frame's CV slot; plain zend_hash_str_del rips the bucket out without
    // decref'ing through the indirection, orphaning the live slot. _del_ind
    // follows the indirection (and is a plain delete for a direct entry),
    // matching the engine's own zend_delete_global_variable.
    zend_hash_str_del_ind(&EG(symbol_table), "_SESSION",
                          sizeof("_SESSION") - 1);
}
// Runs per-request PHP work under a zend_bailout guard. exit()/die() in
// PHP 8.4+ are not bailouts - they land in EG(exception) as an unwind-exit
// (Zend/zend_exceptions.c), which the exception block below classifies.
int rapira_run_handler(zend_fcall_info *fci, zend_fcall_info_cache *fcc) {
    int outcome = OK;
    zval retval;
    ZVAL_UNDEF(&retval);
    fci->size = sizeof *fci;
    // fci does not outlive this frame, so pointing it at a local is fine.
    // cppcheck-suppress autoVariables
    fci->retval = &retval;
    fci->param_count = 0;
    fci->named_params = NULL;

    // _zend_bailout is CG(unclean_shutdown)'s only request-time setter
    // (zend.c:1264) and init_compiler at cycle startup its only reset
    // (zend_compile.c:461) - a 0->1 flip during this run proves a bailout.
    // Read BEFORE the handler/shutdown-fn/destructor calls: a swallowed bailout
    // has already set it by the time control reaches the tail.
    bool clean_at_entry = (!CG(unclean_shutdown)) != 0;

    zend_execute_data *observed_base = EG(current_observed_frame);
    RAPIRA_GUARD(
        {
            zend_call_function(fci, fcc);
            zval_ptr_dtor(&retval);
        },
        outcome, observed_base);

    zend_try {
        // exit()/die() and uncaught exceptions land in EG(exception), not
        // bailouts
        if (EG(exception)) {
            if (zend_is_unwind_exit(EG(exception)) ||
                zend_is_graceful_exit(EG(exception))) {
                outcome = EXIT;
                zend_clear_exception();
            } else {
                // run the userland set_exception_handler; the zend_try guards
                // against a bailout thrown inside that handler.
                zend_try_exception_handler();
                if (EG(exception)) {
                    outcome = THROW;
                    // Throwable path is E_DONT_BAIL and releases the object
                    // itself
                    zend_exception_error(EG(exception), E_ERROR);
                }
            }
        }
    }
    zend_catch {
        outcome = BAILOUT;
        rapira_observer_end_to(
            observed_base); // close any frames the bailout skipped
    }
    zend_end_try();

    RAPIRA_OBSERVER_CLOSE(php_call_shutdown_functions(), observed_base);
    RAPIRA_OBSERVER_CLOSE(zend_call_destructors(), observed_base);

    if (EG(exception)) {
        zend_clear_exception();
    }

    php_free_shutdown_functions();
    gc_protect(0); // reset gc_protect to 0, in case _zend_bailout left it at 1

    if (outcome != BAILOUT && clean_at_entry && CG(unclean_shutdown)) {
        outcome = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    return outcome;
}

int rapira_request_activate(void) {
    int outcome = OK;
    zend_try {
        php_output_activate();
        sapi_activate();
        rapira_modules_request(true);
        rapira_request_init();
        rapira_reset_super_global();
        rapira_activate_auto_globals();
    }
    zend_catch { outcome = BAILOUT; }
    zend_end_try();

    if (outcome == BAILOUT) {
        gc_protect(0);
    }

    return outcome;
}

// per-request sapi teardown, returns 1 if any of the methods bailed out,
// 0 otherwise.
// main/main.c:1985,2002,2031 (source)
int rapira_request_teardown(void) {
    int bailed = OK;
    // a bailout here abandons the observer frames of everything it longjmps
    // over. php_request_shutdown's zend_observer_fcall_end_all() only reaches
    // them while the VM stack still holds them, and the PHP worker loop pops
    // that stack the moment HttpHandler::handleRequest returns - close them
    // here, where they're intact
    zend_execute_data *observed_base = EG(current_observed_frame);

    RAPIRA_GUARD(php_output_end_all(), bailed, observed_base);
    RAPIRA_GUARD(rapira_modules_request(false), bailed, observed_base);
    RAPIRA_GUARD(rapira_reset_session(), bailed, observed_base);
    RAPIRA_GUARD(php_output_deactivate(), bailed, observed_base);
    RAPIRA_GUARD(sapi_deactivate(), bailed, observed_base);

    zend_try { zend_unset_timeout(); }
    zend_end_try();

    // _zend_bailout leaves gc_protect(1); reset unconditionally or the next
    // request runs with cycle GC disabled
    gc_protect(0);

    SG(request_info).request_method = NULL;
    SG(request_info).query_string = NULL;
    SG(request_info).request_uri = NULL;
    SG(request_info).path_translated = NULL;
    SG(request_info).content_type = NULL;
    SG(request_info).cookie_data = NULL;
    SG(request_info).current_user = NULL;
    SG(request_info).content_type_dup = NULL;

    return bailed;
}

// Free the captured last error so it doesn't pin request objects across jobs
// and doesn't trip core_globals_dtor's ZEND_ASSERT(!last_error_message) at
// shutdown (main/main.c:2099).
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
    // shutdown_executor frees this every request; worker mode skips it, so the
    // captured trace (frame args) pins request objects across jobs. 8.5-only+
    // field.
    zend_try {
        zval_ptr_dtor(&EG(last_fatal_error_backtrace));
        ZVAL_UNDEF(&EG(last_fatal_error_backtrace));
    }
    zend_catch { ZVAL_UNDEF(&EG(last_fatal_error_backtrace)); }
    zend_end_try();
#endif
}

// once per process, before sapi_startup
void rapira_process_init(void) {
    // ext_functions[] is static to rapira_classes.c and .functions needs a
    // constant initializer, so wire it up here - before php_module_startup
    // reads the entry.
    rapira_module_entry.functions = rapira_php_functions();

#if defined(SIGPIPE) && defined(SIG_IGN)
    // Ignore SIGPIPE so writes to a hung-up client return EPIPE instead of
    // killing the worker. signal() only fails with EINVAL, which
    // SIGPIPE/SIG_IGN cannot hit; abort rather than run a worker any client
    // could kill.
    if (signal(SIGPIPE, SIG_IGN) == SIG_ERR) {
        perror("rapira: signal(SIGPIPE, SIG_IGN)");
        abort();
    }
#endif
    zend_signal_startup();
}

// Once per forked worker, before any PHP runs in the child. The Zend MM heap
// was created by the master's MINIT; 8.5+ requires the child to re-key it
// (debug builds assert getpid() == heap->pid in zend_mm_shutdown) and re-arm
// the per-process execution timer, which fork does not inherit.
void rapira_child_init(void) {
#if PHP_VERSION_ID >= 80500
    php_child_init();
#endif
}

// Temp streams (POST request_body) are only NULLed by sapi_deactivate_module();
// nothing reclaims the resource in a resident request, so sweep dead ones
// before serving the next job. Safe here: the previous request is finished.
void rapira_release_temporary_streams(void) {
    zend_resource *val = NULL;
    int stream_type = php_file_le_stream();
    ZEND_HASH_FOREACH_PTR(&EG(regular_list), val) {
        if (val->type == stream_type) {
            php_stream *stream = val->ptr;
            if (stream != NULL && stream->ops == &php_stream_temp_ops &&
                stream->__exposed == 0 && GC_REFCOUNT(val) == 1) {
                zend_list_delete(val);
            }
        }
    }
    ZEND_HASH_FOREACH_END();
}

// Full teardown for the worker restart loop. The only unguarded step inside
// php_request_shutdown is zend_observer_fcall_end_all (main.c:1971); it NULLs
// EG(current_observed_frame) before walking (zend_observer.c:322), so on a
// bailout the retry skips it and finishes the remaining teardown steps.
int rapira_request_shutdown(void) {
    volatile int bailed = OK;
    rapira_job_timeout = -1; // cycle over: next cycle re-captures its budget
#ifdef HAVE_PHP_SESSION
    // a bailout inside a user save handler skips the cleanup that clears
    // PS(in_save_handler); the module RSHUTDOWN's recursion guard then refuses
    // to run the handler's close() and whatever it holds leaks (mod_user.c:29)
    PS(in_save_handler) = 0;
#endif
    zend_try { php_request_shutdown(NULL); }
    zend_catch {
        bailed = BAILOUT;
        zend_try { php_request_shutdown(NULL); }
        zend_end_try();
    }
    zend_end_try();
    return bailed;
}
