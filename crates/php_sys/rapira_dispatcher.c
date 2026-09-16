#include "rapira_classes.h"

#include "wrapper.h"
#include "zend.h"
#include "zend_API.h"
#include "zend_enum.h"
#include "zend_exceptions.h"

// rust glue; the verbs throw from Rust and report false with the throw pending
extern const char *rapira_rs_version(size_t *len);
extern void rapira_rs_log_call(zend_string *message, zend_object *level,
                               HashTable *context);
extern bool rapira_rs_receive(int64_t timeout_us, zval *return_value);
extern bool rapira_rs_try_receive(zval *return_value);
extern bool rapira_rs_dispatcher_info(zval *return_value);
extern bool rapira_rs_get_dispatcher(zval *return_value);
extern bool rapira_rs_trace_context(zval *return_value);
extern int rapira_rs_handle_request(zend_fcall_info *fci,
                                    zend_fcall_info_cache *fcc);

int rapira_mode = RAPIRA_MODE_CLASSIC;

ZEND_FUNCTION(Rapira_get_version) {
    ZEND_PARSE_PARAMETERS_NONE();

    size_t len = 0;
    const char *version = rapira_rs_version(&len);
    RETURN_STRINGL(version, len);
}

ZEND_FUNCTION(Rapira_trace_context) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_trace_context(return_value)) {
        rapira_throw_or_backstop("trace_context");
        RETURN_THROWS();
    }
}

// rapira_mode is set once before the PHP thread starts (start.rs), so the case is stable for the process
ZEND_FUNCTION(Rapira_get_mode) {
    ZEND_PARSE_PARAMETERS_NONE();

    const char *name;
    switch (rapira_mode) {
    case RAPIRA_MODE_WORKER:
        name = "Worker";
        break;
    case RAPIRA_MODE_DISPATCHER:
        name = "Dispatcher";
        break;
    default:
        name = "Classic";
        break;
    }

    // the case object belongs to the per-request class constant; the copy takes a reference
    RETURN_OBJ_COPY(zend_enum_get_case_cstr(rapira_ce_mode, name));
}

ZEND_FUNCTION(Rapira_get_dispatcher) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_get_dispatcher(return_value)) {
        rapira_throw_or_backstop("get_dispatcher");
        RETURN_THROWS();
    }
}

// a nested handle_request() would rebind SG(server_context) over the live job
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
            "no host hands jobs to this process outside worker mode", 0);
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
    (void)return_value;
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

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, name) {
    ZEND_PARSE_PARAMETERS_NONE();
    // the plugin's root TOML section
    RETURN_STRING("http");
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, __construct) {
    (void)execute_data;
    (void)return_value;
    zend_throw_error(NULL,
                     "host-created; obtain it from \\Rapira\\get_dispatcher()");
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, __construct) {
    (void)execute_data;
    (void)return_value;
    zend_throw_error(NULL, "host-created");
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, receive) {
    zend_long timeout = -1;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_LONG(timeout)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_receive((int64_t)timeout, return_value)) {
        rapira_throw_or_backstop("receive");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, tryReceive) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_try_receive(return_value)) {
        rapira_throw_or_backstop("tryReceive");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, getInfo) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_dispatcher_info(return_value)) {
        rapira_throw_or_backstop("getInfo");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, pendingCount) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->pending);
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, activeCount) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->active);
}
