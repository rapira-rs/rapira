#include "rapira_sapi.h"

#include "rapira_arginfo.h"
#include "rapira_exception_arginfo.h"

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

// own copies: std_object_handlers is shared engine state. A plugin's
// Dispatcher class gets std handlers without clone, and its DispatcherInfo
// class gets the rapira_dispatcher_info_obj layout.
static zend_object_handlers rapira_dispatcher_handlers;
static zend_object_handlers rapira_info_handlers;

void (*const *rapira_plugin_registers)(void);
size_t rapira_plugin_register_count;

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

    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(rapira_ce_inet_address, obj, ZEND_STRL("ip"), ip);
    zend_update_property_long(rapira_ce_inet_address, obj, ZEND_STRL("port"),
                              port);
}

ZEND_METHOD(Rapira_UnixAddress, __construct) {
    zend_string *path;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR_OR_NULL(path)
    ZEND_PARSE_PARAMETERS_END();

    rapira_prop_str_or_null(rapira_ce_unix_address, Z_OBJ_P(ZEND_THIS),
                            ZEND_STRL("path"), path);
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

    zend_class_entry *ce = rapira_ce_tls;
    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, obj, ZEND_STRL("version"), version);
    zend_update_property_str(ce, obj, ZEND_STRL("cipher"), cipher);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("negotiatedProtocol"),
                            negotiated);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("requestedServerName"),
                            server_name);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("certSerial"), serial);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("certOrganization"), org);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("certFingerprint"), fingerprint);
}

bool rapira_address_arg(zval *zv, uint32_t num) {
    if (Z_TYPE_P(zv) == IS_OBJECT &&
        (instanceof_function(Z_OBJCE_P(zv), rapira_ce_inet_address) ||
         instanceof_function(Z_OBJCE_P(zv), rapira_ce_unix_address))) {
        return true;
    }
    zend_argument_type_error(
        num,
        "must be of type Rapira\\InetAddress|Rapira\\UnixAddress, %s given",
        zend_zval_value_name(zv));
    return false;
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

// Reflection does not skip the constructor of a final class with
// create_object, so only get_dispatcher() creates the Dispatcher object.
void rapira_plugin_wire(const rapira_plugin *plugin) {
    plugin->dispatcher->create_object = rapira_object_create;
    plugin->dispatcher->default_object_handlers = &rapira_dispatcher_handlers;
    plugin->info->create_object = rapira_object_create;
    plugin->info->default_object_handlers = &rapira_info_handlers;
}

// the plugin classes extend the base classes, so the base classes register
// first
PHP_MINIT_FUNCTION(rapira) {
    rapira_register_classes();
    for (size_t i = 0; i < rapira_plugin_register_count; i++) {
        rapira_plugin_registers[i]();
    }
    return SUCCESS;
}

// The quiesce hook runs after the shutdown functions, the destructors, the
// output flush and the timer stop, and before the memory manager shutdown:
// https://github.com/php/php-src/blob/php-8.5.11/main/main.c#L1974-L2050
// A write to a held unit from a later RSHUTDOWN would lend a string that the
// memory manager then frees, so user code there must not write.
PHP_RSHUTDOWN_FUNCTION(rapira) {
    if (rapira_plugin_current != NULL &&
        rapira_plugin_current->quiesce != NULL) {
        rapira_plugin_current->quiesce();
    }
#ifdef HAVE_PHP_SESSION
    // the session RSHUTDOWN runs after this one (zend_collect_module_handlers):
    // write the session before the response leaves
    if (PS(session_status) == php_session_active) {
        zend_try { php_session_flush(1); }
        zend_end_try();
    }
#endif
    php_header();
    rapira_rs_finish_response();
    rapira_dispatcher_release();
    return SUCCESS;
}

// The module entry lives here: ext_functions is file-static in
// rapira_arginfo.h, and only this file includes it.
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
