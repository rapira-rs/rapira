#include "rapira_sapi.h"
#include "ext/spl/spl_exceptions.h"
#include "rapira_arginfo.h"
#include "rapira_exception_arginfo.h"
#include "zend_API.h"
#include "zend_exceptions.h"
#include "zend_object_handlers.h"
#include "zend_objects.h"
#include "zend_objects_API.h"
#include "zend_property_hooks.h"
#include "zend_types.h"

// rust glue (src/values.rs): these return false with a PHP exception already pending
extern bool rapira_rs_ctor_inet_address(zend_object *obj, zend_string *ip,
                                        int64_t port);
extern bool rapira_rs_ctor_unix_address(zend_object *obj, zend_string *path);
extern bool rapira_rs_ctor_tls(zend_object *obj, zend_string *version,
                               zend_string *cipher, zend_string *negotiated,
                               zend_string *server_name, zend_string *serial,
                               zend_string *org, zend_string *fingerprint);

zend_class_entry *rapira_ce_throwable;
zend_class_entry *rapira_ce_work;
zend_class_entry *rapira_ce_dispatcher;
zend_class_entry *rapira_ce_dispatcher_info;
zend_class_entry *rapira_ce_log_level;
zend_class_entry *rapira_ce_mode;

zend_class_entry *rapira_ce_closed_exception;
zend_class_entry *rapira_ce_timeout_exception;
zend_class_entry *rapira_ce_work_discarded_exception;
zend_class_entry *rapira_ce_no_dispatcher_error;
zend_class_entry *rapira_ce_not_in_worker_mode_error;
zend_class_entry *rapira_ce_already_finalized_error;

zend_class_entry *rapira_ce_inet_address;
zend_class_entry *rapira_ce_unix_address;
zend_class_entry *rapira_ce_tls;

const zend_function_entry *rapira_php_functions(void) { return ext_functions; }

// own copies: std_object_handlers is shared engine state
zend_object_handlers rapira_host_handlers;
zend_object_handlers rapira_info_handlers;

zend_object *rapira_dispatcher_info_create(zend_class_entry *ce) {
    rapira_dispatcher_info_obj *obj = zend_object_alloc(sizeof(*obj), ce);
    obj->pending = 0;
    obj->active = 0;
    zend_object_std_init(&obj->std, ce);
    object_properties_init(&obj->std, ce);
    return &obj->std;
}

ZEND_METHOD(Rapira_InetAddress, __construct) {
    zend_string *ip;
    zend_long port;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(ip)
    Z_PARAM_LONG(port)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_inet_address(Z_OBJ_P(ZEND_THIS), ip, (int64_t)port)) {
        rapira_throw_or_backstop("InetAddress construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_UnixAddress, __construct) {
    zend_string *path;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR_OR_NULL(path)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_unix_address(Z_OBJ_P(ZEND_THIS), path)) {
        rapira_throw_or_backstop("UnixAddress construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Tls, __construct) {
    zend_string *version, *cipher, *negotiated, *server_name, *serial, *org,
        *fingerprint;
    ZEND_PARSE_PARAMETERS_START(7, 7)
    Z_PARAM_STR(version)
    Z_PARAM_STR(cipher)
    Z_PARAM_STR_OR_NULL(negotiated)
    Z_PARAM_STR_OR_NULL(server_name)
    Z_PARAM_STR_OR_NULL(serial)
    Z_PARAM_STR_OR_NULL(org)
    Z_PARAM_STR_OR_NULL(fingerprint)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_tls(Z_OBJ_P(ZEND_THIS), version, cipher, negotiated,
                            server_name, serial, org, fingerprint)) {
        rapira_throw_or_backstop("Tls construction");
        RETURN_THROWS();
    }
}

void rapira_register_classes(void) {
    rapira_ce_throwable =
        register_class_Rapira_Exception_RapiraThrowable(zend_ce_throwable);

    rapira_ce_closed_exception =
        register_class_Rapira_Exception_ClosedException(spl_ce_RuntimeException,
                                                        rapira_ce_throwable);
    rapira_ce_timeout_exception =
        register_class_Rapira_Exception_TimeoutException(
            spl_ce_RuntimeException, rapira_ce_throwable);
    rapira_ce_work_discarded_exception =
        register_class_Rapira_Exception_WorkDiscardedException(
            spl_ce_RuntimeException, rapira_ce_throwable);
    rapira_ce_no_dispatcher_error =
        register_class_Rapira_Exception_NoDispatcherError(zend_ce_error,
                                                          rapira_ce_throwable);
    rapira_ce_not_in_worker_mode_error =
        register_class_Rapira_Exception_NotInWorkerModeError(
            zend_ce_error, rapira_ce_throwable);
    rapira_ce_already_finalized_error =
        register_class_Rapira_Exception_AlreadyFinalizedError(
            zend_ce_error, rapira_ce_throwable);

    rapira_ce_log_level = register_class_Rapira_LogLevel();
    rapira_ce_mode = register_class_Rapira_Mode();
    rapira_ce_work = register_class_Rapira_Work();
    rapira_ce_dispatcher_info = register_class_Rapira_DispatcherInfo();
    rapira_ce_dispatcher = register_class_Rapira_Dispatcher();

    rapira_ce_inet_address = register_class_Rapira_InetAddress();
    rapira_ce_unix_address = register_class_Rapira_UnixAddress();
    rapira_ce_tls = register_class_Rapira_Tls();

    // clone_obj = NULL: engine throws on clone (Zend/zend_vm_def.h:6050-6056)
    memcpy(&rapira_host_handlers, &std_object_handlers,
           sizeof(rapira_host_handlers));
    rapira_host_handlers.clone_obj = NULL;

    memcpy(&rapira_info_handlers, &std_object_handlers,
           sizeof(rapira_info_handlers));
    rapira_info_handlers.clone_obj = NULL;
    rapira_info_handlers.offset = offsetof(rapira_dispatcher_info_obj, std);
}
