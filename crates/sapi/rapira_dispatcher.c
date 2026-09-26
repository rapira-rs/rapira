#include "rapira_sapi.h"
#include "zend.h"
#include "zend_API.h"
#include "zend_enum.h"
#include "zend_exceptions.h"

// rust glue; the verbs throw from Rust and report false with the throw pending
extern void rapira_rs_log_call(zend_string *message, zend_object *level,
                               HashTable *context);
extern bool rapira_rs_receive(int64_t timeout_us, zval *return_value);
extern bool rapira_rs_try_receive(zval *return_value);
extern bool rapira_rs_dispatcher_info(zval *return_value);
extern bool rapira_rs_get_dispatcher(zval *return_value);
extern int rapira_rs_handle_request(zend_fcall_info *fci,
                                    zend_fcall_info_cache *fcc);

int rapira_mode = RAPIRA_MODE_CLASSIC;

ZEND_FUNCTION(Rapira_get_version) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_STRINGL(RAPIRA_VERSION, sizeof(RAPIRA_VERSION) - 1);
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

void rapira_sapi_receive(INTERNAL_FUNCTION_PARAMETERS) {
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

void rapira_sapi_try_receive(INTERNAL_FUNCTION_PARAMETERS) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_try_receive(return_value)) {
        rapira_throw_or_backstop("tryReceive");
        RETURN_THROWS();
    }
}

void rapira_sapi_get_info(INTERNAL_FUNCTION_PARAMETERS) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_dispatcher_info(return_value)) {
        rapira_throw_or_backstop("getInfo");
        RETURN_THROWS();
    }
}

void rapira_sapi_pending_count(INTERNAL_FUNCTION_PARAMETERS) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->pending);
}

void rapira_sapi_active_count(INTERNAL_FUNCTION_PARAMETERS) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->active);
}
