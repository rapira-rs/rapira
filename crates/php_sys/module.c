#include "php.h"
#include "rapira_classes.h"
#include "wrapper.h"
#include "zend_types.h"

// injected by build.rs
#ifndef RAPIRA_VERSION
#define RAPIRA_VERSION "0.0.0-dev"
#endif

extern void rapira_rs_finish_response(void);

// php_handle_aborted_connection (main.c:2722) longjmps past Rust's catch_unwind
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

// On bailout: flag it and close the observer frames the longjmp abandoned.
#define RAPIRA_GUARD(stmt, flag, base)                                         \
    zend_try { stmt; }                                                         \
    zend_catch {                                                               \
        (flag) = BAILOUT;                                                      \
        rapira_observer_end_to(base);                                          \
    }                                                                          \
    zend_end_try()

// Like RAPIRA_GUARD, but records no outcome: only closes observer frames.
#define RAPIRA_OBSERVER_CLOSE(stmt, base)                                      \
    zend_try { stmt; }                                                         \
    zend_catch { rapira_observer_end_to(base); }                               \
    zend_end_try()

// php_output_end_all flushes userland handlers, which can fatal and bailout.
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
    if (rapira_mode == RAPIRA_MODE_DISPATCHER) {
        // ungated it would tear down userland ob buffers into the log
        zend_throw_error(
            NULL, "rapira_finish_request() is not available in dispatcher "
                  "mode; finalize through the Exchange");
        RETURN_THROWS();
    }
    if (rapira_finish_output() != OK) {
        // re-raise: rapira_run_handler classifies it, 500s and recycles
        zend_bailout();
    }
    rapira_rs_finish_response();
    RETURN_TRUE;
}

PHP_MINIT_FUNCTION(rapira) {
    (void)type;
    (void)module_number;
    rapira_register_classes();
    return SUCCESS;
}

PHP_RSHUTDOWN_FUNCTION(rapira) {
    (void)type;
    (void)module_number;
    rapira_rs_dispatcher_release();
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
    NULL,
    RAPIRA_VERSION,
    STANDARD_MODULE_PROPERTIES};

// ext/filter frees its cached raw input only in RSHUTDOWN (filter.c:190-196).
// PECL imap frees its error and alert stacks only in RSHUTDOWN; without the reload a resident worker leaks and request N reads request N-1's errors.
static const char *RELOAD_MODULES[] = {"filter", "imap", NULL};

static void rapira_modules_request(bool startup) {
    zend_module_entry *module = NULL;
    for (const char **name = RELOAD_MODULES; *name; name++) {
        module = zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
        if (!module) {
            continue;
        }
        if (startup && module->request_startup_func) {
            // RINIT failure is fatal upstream (zend_activate_modules exit(1)s)
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

// Close frames above base only: end_all() would also close the resident frames.
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

// Per-job budget; -1 = not captured. set_time_limit() rewrites the live field.
static zend_long rapira_job_timeout = -1;

// zend_unset_timeout no-ops when EG(timeout_seconds) is 0, so disarm first.
void rapira_receive_untimed(void) {
    if (rapira_job_timeout < 0) {
        rapira_job_timeout = EG(timeout_seconds);
    }
    zend_unset_timeout();
    EG(timeout_seconds) = 0;
}

// zend_set_timeout re-assigns EG(timeout_seconds) itself (zend_execute_API.c).
void rapira_receive_timed(void) { zend_set_timeout(rapira_job_timeout, false); }

// Per-request state php_request_startup() resets that the worker path skips.
static void rapira_request_init(void) {
    PG(connection_status) = PHP_CONNECTION_NORMAL;
    PG(header_is_being_sent) = 0;
    // a fatal can strand these set, breaking later URL opens and error logging
    PG(in_error_log) = false;
    PG(in_user_include) = false;
    // init_compiler clears this per cycle (zend_compile.c:461), not per job
    CG(unclean_shutdown) = false;

    // reset_signals=0: the SIGRTMIN handler is installed process-wide at boot
    if (rapira_job_timeout < 0) {
        rapira_job_timeout = EG(timeout_seconds);
    }
    zend_set_timeout(rapira_job_timeout, false);

    if (PG(expose_php)) {
        sapi_add_header(SAPI_PHP_VERSION_HEADER,
                        sizeof(SAPI_PHP_VERSION_HEADER) - 1, 1);
    }

    // 8.6: output_handler is a zend_string*, empty is NULL (php-src e0221be8)
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

// sapi_activate re-arms CG(auto_globals) per request; worker mode skips it.
static void rapira_activate_auto_globals(void) {
    zend_auto_global *auto_global = NULL;
    zend_string *_env = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_ENV);

    // skip $_ENV: its create callback dtors the array before variables_order
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
// Unguarded on purpose: a bailing save handler must reach the caller's catch.
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
    // php_rinit_session_globals() scrubs these per request; worker skips RINIT
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
// exit()/die() in 8.4+ are not bailouts: they land in EG(exception).
int rapira_run_handler(zend_fcall_info *fci, zend_fcall_info_cache *fcc) {
    int outcome = OK;
    zval retval;
    ZVAL_UNDEF(&retval);
    fci->size = sizeof *fci;
    // fci does not outlive this frame
    // cppcheck-suppress autoVariables
    fci->retval = &retval;
    fci->param_count = 0;
    fci->named_params = NULL;

    // only _zend_bailout sets it mid-request (zend.c:1264): 0->1 proves bailout
    bool unclean_at_entry = CG(unclean_shutdown);

    zend_execute_data *observed_base = EG(current_observed_frame);
    RAPIRA_GUARD(
        {
            zend_call_function(fci, fcc);
            zval_ptr_dtor(&retval);
        },
        outcome, observed_base);

    zend_try {
        if (EG(exception)) {
            if (zend_is_unwind_exit(EG(exception)) ||
                zend_is_graceful_exit(EG(exception))) {
                outcome = EXIT;
                zend_clear_exception();
            } else {
                // the zend_try contains a bailout from the userland handler
                zend_try_exception_handler();
                if (EG(exception)) {
                    outcome = THROW;
                    // Throwable path is E_DONT_BAIL and releases the object
                    zend_exception_error(EG(exception), E_ERROR);
                }
            }
        }
    }
    zend_catch {
        outcome = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    zend_end_try();

    // No destructor sweep per job: it would run __destruct on live objects
    RAPIRA_OBSERVER_CLOSE(php_call_shutdown_functions(), observed_base);
    // freeing the table releases closure captures, which can run __destruct
    RAPIRA_OBSERVER_CLOSE(php_free_shutdown_functions(), observed_base);

    // must follow every step that runs userland code, or it rethrows later
    if (EG(exception)) {
        zend_clear_exception();
    }

    gc_protect(false); // _zend_bailout can leave it engaged

    if (outcome != BAILOUT && !unclean_at_entry && CG(unclean_shutdown)) {
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
        gc_protect(false);
    }

    return outcome;
}

// sapi_activate resets the slot without releasing (SAPI.c), so it leaks per job
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

// per-request sapi teardown (main/main.c:1985,2002,2031)
int rapira_request_teardown(void) {
    int bailed = OK;
    // the VM stack is popped when handleRequest returns, so close frames here
    zend_execute_data *observed_base = EG(current_observed_frame);

    RAPIRA_GUARD(php_output_end_all(), bailed, observed_base);
    RAPIRA_GUARD(rapira_modules_request(false), bailed, observed_base);
    RAPIRA_GUARD(rapira_reset_session(), bailed, observed_base);
    RAPIRA_GUARD(php_output_deactivate(), bailed, observed_base);
    RAPIRA_GUARD(rapira_release_header_callback(), bailed, observed_base);
    RAPIRA_GUARD(sapi_deactivate(), bailed, observed_base);

    zend_try { zend_unset_timeout(); }
    zend_end_try();

    // _zend_bailout leaves gc_protect engaged, disabling GC for the next job
    gc_protect(false);

    // teardown can run __destruct; a pending throw surfaces in the next job
    if (EG(exception)) {
        zend_clear_exception();
    }

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

// A kept last error pins objects and trips core_globals_dtor (main.c:2102).
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
    // the captured trace pins request objects; only shutdown_executor frees it.
    // the dtor is valid only inside a live request; after php_request_shutdown,
    // rapira_request_shutdown scrubs the stale zval instead
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
    // ext_functions[] is file-static, so wire it up before php_module_startup
    rapira_module_entry.functions = rapira_php_functions();

#if defined(SIGPIPE) && defined(SIG_IGN)
    // Ignore SIGPIPE so writes to a hung-up client return EPIPE, not a signal.
    if (signal(SIGPIPE, SIG_IGN) == SIG_ERR) {
        perror("rapira: signal(SIGPIPE, SIG_IGN)");
        abort();
    }
#endif
    zend_signal_startup();
}

// re-key the MM heap after fork; 8.5 asserts getpid() == heap->pid at shutdown
void rapira_child_init(void) {
#if PHP_VERSION_ID >= 80500
    refresh_memory_manager();
#endif
}

// sapi_deactivate_module only NULLs temp streams; nothing reclaims the resource
void rapira_release_temporary_streams(void) {
    zend_resource *val = NULL;
    int stream_type = php_file_le_stream();
    ZEND_HASH_FOREACH_PTR(&EG(regular_list), val) {
        if (val->type == stream_type) {
            php_stream *stream = val->ptr;
            if (stream != NULL && stream->ops == &php_stream_temp_ops &&
                !(stream->flags & PHP_STREAM_FLAG_NO_FCLOSE) &&
                stream->__exposed == 0 && GC_REFCOUNT(val) == 1) {
                zend_list_delete(val);
            }
        }
    }
    ZEND_HASH_FOREACH_END();
}

// Boot-registered shutdown functions run once, at cycle end.
static HashTable *rapira_boot_shutdown_functions = NULL;

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
            // only register_user_shutdown_function() keys by name
            // (ext/session); userland register_shutdown_function() appends with
            // a numeric key
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

// retry is safe: end_all NULLs EG(current_observed_frame) (zend_observer.c:322)
int rapira_request_shutdown(void) {
    volatile int bailed = OK;
    // put the budget back armed: a stale 0 disables max_execution_time next
    // cycle, and the boot shutdown functions run under the timer until
    // php_request_shutdown disarms it (main/main.c:1993)
    if (rapira_job_timeout >= 0) {
        zend_set_timeout(rapira_job_timeout, false);
    }
    rapira_job_timeout = -1; // cycle over: next cycle re-captures its budget
#ifdef HAVE_PHP_SESSION
    // left set, it makes RSHUTDOWN skip the handler's close() (mod_user.c:29)
    PS(in_save_handler) = false;
#endif
    // the restore sits inside the try: its hash inserts allocate, and a bailout
    // with no jump target set is exit(-1) (zend.c:1258)
    zend_try {
        rapira_restore_boot_shutdown_functions();
        php_request_shutdown(NULL);
    }
    zend_catch {
        bailed = BAILOUT;
        zend_try { php_request_shutdown(NULL); }
        zend_end_try();
    }
    zend_end_try();
#if PHP_VERSION_ID >= 80500
    // fast shutdown skips the backtrace release (zend_execute_API.c:282,309)
    // and the arena free reclaimed it; drop the stale zval without a dtor
    ZVAL_UNDEF(&EG(last_fatal_error_backtrace));
#endif
    return bailed;
}
