#include "rapira_sapi.h"

// rust glue; a verb that reports false has a throw pending, except an empty
// tryReceive, which reports false with no throw
extern void rapira_rs_log_call(zend_string *message, zend_object *level,
                               HashTable *context);
extern bool rapira_rs_receive(int64_t timeout_us, rapira_held *held);
extern bool rapira_rs_try_receive(rapira_held *held);
extern void rapira_rs_dispatcher_counts(zend_long *pending, zend_long *active);
extern bool rapira_rs_held_is_finalized(const rapira_held *held);
extern bool rapira_rs_held_is_cancelled(const rapira_held *held);
extern int rapira_rs_handle_request(zend_fcall_info *fci,
                                    zend_fcall_info_cache *fcc);

int rapira_mode = RAPIRA_MODE_CLASSIC;

zend_class_entry *rapira_plugin_ce_dispatcher;
zend_class_entry *rapira_plugin_ce_info;
zend_class_entry *rapira_plugin_ce_unit;
const char *rapira_plugin_busy;

// the memoized plugin Dispatcher of this cycle; RSHUTDOWN releases it
static zval rapira_dispatcher_memo;

ZEND_FUNCTION(Rapira_get_version) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_STRINGL(RAPIRA_VERSION, sizeof(RAPIRA_VERSION) - 1);
}

// rapira_mode is set once before the PHP thread starts (start.rs), so the case is stable for the process
ZEND_FUNCTION(Rapira_get_mode) {
    ZEND_PARSE_PARAMETERS_NONE();

    static const char *const names[] = {
        [RAPIRA_MODE_CLASSIC] = "Classic",
        [RAPIRA_MODE_WORKER] = "Worker",
        [RAPIRA_MODE_DISPATCHER] = "Dispatcher",
    };

    // the case object belongs to the per-request class constant; the copy takes a reference
    RETURN_OBJ_COPY(
        zend_enum_get_case_cstr(rapira_ce_mode, names[rapira_mode]));
}

ZEND_FUNCTION(Rapira_get_dispatcher) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (rapira_mode != RAPIRA_MODE_DISPATCHER) {
        zend_throw_exception(rapira_ce_no_dispatcher_error,
                             "Dispatcher is only available in Mode::Dispatcher",
                             0);
        RETURN_THROWS();
    }
    if (Z_ISUNDEF(rapira_dispatcher_memo)) {
        object_init_ex(&rapira_dispatcher_memo, rapira_plugin_ce_dispatcher);
    }
    RETURN_COPY(&rapira_dispatcher_memo);
}

// RSHUTDOWN calls it after the shutdown functions
void rapira_dispatcher_release(void) {
    zval_ptr_dtor(&rapira_dispatcher_memo);
    ZVAL_UNDEF(&rapira_dispatcher_memo);
}

// A bailout in RSHUTDOWN skips the release. The object store teardown frees
// the object, so this function clears the zval and calls no dtor.
void rapira_dispatcher_forget(void) { ZVAL_UNDEF(&rapira_dispatcher_memo); }

// a nested handle_request() would rebind SG(server_context) over the live unit
static bool rapira_in_handle_request = false;

ZEND_FUNCTION(Rapira_handle_request) {
    zend_fcall_info fci;
    zend_fcall_info_cache fcc;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_FUNC(fci, fcc)
    ZEND_PARSE_PARAMETERS_END();

    if (rapira_mode != RAPIRA_MODE_WORKER) {
        zend_throw_exception(
            rapira_ce_not_in_worker_mode_error,
            "Requests are only handed out in Mode::Worker", 0);
        RETURN_THROWS();
    }
    if (rapira_in_handle_request) {
        zend_throw_error(
            NULL, "handle_request() may not be called from inside its handler");
        RETURN_THROWS();
    }
    rapira_in_handle_request = true;
    int action = rapira_rs_handle_request(&fci, &fcc);
    rapira_in_handle_request = false;
    if (action == RAPIRA_HANDLE_RECYCLE) {
        // response sealed in Rust; unwind so no PHP runs post-longjmp
        zend_bailout();
    }
    RETURN_BOOL(action == RAPIRA_HANDLE_CONTINUE);
}

ZEND_FUNCTION(Rapira_log) {
    zend_string *message = NULL;
    zval *level = NULL;
    HashTable *context = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 3)
    Z_PARAM_STR(message)
    Z_PARAM_OPTIONAL
    // the log level is a PHP enum: parsed as an object of that class
    Z_PARAM_OBJECT_OF_CLASS(level, rapira_ce_log_level)
    Z_PARAM_ARRAY_HT(context)
    ZEND_PARSE_PARAMETERS_END();

    rapira_rs_log_call(message, level ? Z_OBJ_P(level) : NULL, context);

    // log() never throws: drop an exception from a context jsonSerialize()
    if (EG(exception) && !zend_is_unwind_exit(EG(exception)) &&
        !zend_is_graceful_exit(EG(exception))) {
        zend_clear_exception();
    }
}

// The object exists before the pull, so a unit never waits for an owner.
ZEND_FUNCTION(rapira_sapi_receive) {
    zend_long timeout = -1;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_LONG(timeout)
    ZEND_PARSE_PARAMETERS_END();

    if (timeout < -1) {
        zend_argument_value_error(1, "must be greater than or equal to -1");
        RETURN_THROWS();
    }
    zval obj;
    object_init_ex(&obj, rapira_plugin_ce_unit);
    bool attached =
        rapira_rs_receive((int64_t)timeout, rapira_held_of(Z_OBJ(obj)));
    if (!attached) {
        zval_ptr_dtor(&obj);
    }
    RAPIRA_RETURN_THROWS_UNLESS(attached, "receive");
    RETURN_OBJ(Z_OBJ(obj));
}

ZEND_FUNCTION(rapira_sapi_try_receive) {
    ZEND_PARSE_PARAMETERS_NONE();
    zval obj;
    object_init_ex(&obj, rapira_plugin_ce_unit);
    if (!rapira_rs_try_receive(rapira_held_of(Z_OBJ(obj)))) {
        zval_ptr_dtor(&obj);
        if (EG(exception)) {
            RETURN_THROWS();
        }
        RETURN_NULL();
    }
    RETURN_OBJ(Z_OBJ(obj));
}

ZEND_FUNCTION(rapira_sapi_get_info) {
    ZEND_PARSE_PARAMETERS_NONE();
    object_init_ex(return_value, rapira_plugin_ce_info);
    rapira_dispatcher_info_obj *info =
        rapira_dispatcher_info_from(Z_OBJ_P(return_value));
    rapira_rs_dispatcher_counts(&info->pending, &info->active);
}

ZEND_FUNCTION(rapira_sapi_pending_count) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->pending);
}

ZEND_FUNCTION(rapira_sapi_active_count) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->active);
}

ZEND_FUNCTION(rapira_sapi_is_finalized) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(
        rapira_rs_held_is_finalized(rapira_held_of(Z_OBJ_P(ZEND_THIS))));
}

ZEND_FUNCTION(rapira_sapi_is_cancelled) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(
        rapira_rs_held_is_cancelled(rapira_held_of(Z_OBJ_P(ZEND_THIS))));
}

ZEND_FUNCTION(rapira_sapi_dispatcher_construct) {
    zend_throw_error(NULL,
                     "host-created; obtain it from \\Rapira\\get_dispatcher()");
}

ZEND_FUNCTION(rapira_sapi_host_created) {
    zend_throw_error(NULL, "host-created");
}
