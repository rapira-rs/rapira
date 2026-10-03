#include "rapira_sapi.h"

#include "rapira_arginfo.h"
#include "rapira_exception_arginfo.h"

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

// own copies: std_object_handlers is shared engine state
zend_object_handlers rapira_dispatcher_handlers;
zend_object_handlers rapira_info_handlers;

// create_object for a class whose default_object_handlers set .offset: the C
// prefix sits before std, and zend_object_alloc zeroes it
// (Zend/zend_objects_API.h), so NULL and IS_UNDEF fields need no assignment.
zend_object *rapira_object_create(zend_class_entry *ce) {
    size_t offset = ce->default_object_handlers->offset;
    char *obj = zend_object_alloc(offset + sizeof(zend_object), ce);
    zend_object *std = (zend_object *)(obj + offset);
    zend_object_std_init(std, ce);
    object_properties_init(std, ce);
    return std;
}

ZEND_METHOD(Rapira_InetAddress, __construct) {
    zend_string *ip;
    zend_long port;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(ip)
    Z_PARAM_LONG(port)
    ZEND_PARSE_PARAMETERS_END();

    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_ctor_inet_address(Z_OBJ_P(ZEND_THIS), ip, (int64_t)port),
        "InetAddress construction");
}

ZEND_METHOD(Rapira_UnixAddress, __construct) {
    zend_string *path;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR_OR_NULL(path)
    ZEND_PARSE_PARAMETERS_END();

    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_ctor_unix_address(Z_OBJ_P(ZEND_THIS), path),
        "UnixAddress construction");
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

    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_ctor_tls(Z_OBJ_P(ZEND_THIS), version, cipher, negotiated,
                           server_name, serial, org, fingerprint),
        "Tls construction");
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
    rapira_dispatcher_handlers = std_object_handlers;
    rapira_dispatcher_handlers.clone_obj = NULL;

    rapira_info_handlers = std_object_handlers;
    rapira_info_handlers.clone_obj = NULL;
    rapira_info_handlers.offset = offsetof(rapira_dispatcher_info_obj, std);
}

// start.rs: runs the register function of each plugin part, in the boot order
extern void rapira_rs_register_plugin_classes(void);

// the plugin classes extend the base classes, so the base classes register first
PHP_MINIT_FUNCTION(rapira) {
    rapira_register_classes();
    rapira_rs_register_plugin_classes();
    return SUCCESS;
}

PHP_RSHUTDOWN_FUNCTION(rapira) {
    rapira_rs_dispatcher_release();
    return SUCCESS;
}

// clang-format off
zend_module_entry rapira_module_entry = {
    STANDARD_MODULE_HEADER,
    "rapira",
    ext_functions,
    PHP_MINIT(rapira),
    NULL,
    NULL,
    PHP_RSHUTDOWN(rapira),
    NULL,
    RAPIRA_VERSION,
    STANDARD_MODULE_PROPERTIES};
// clang-format on
