#include "rapira_http.h"
#include "rapira_http_arginfo.h"

zend_class_entry *rapira_ce_http_multipart;
zend_class_entry *rapira_ce_internal_http_dispatcher;
zend_class_entry *rapira_ce_http_form_field;
zend_class_entry *rapira_ce_http_uploaded_file;
zend_class_entry *rapira_ce_http_request;
zend_class_entry *rapira_ce_internal_http_exchange;
zend_class_entry *rapira_ce_internal_http_dispatcher_info;
zend_class_entry *rapira_ce_http_head_already_written_error;
zend_class_entry *rapira_ce_http_head_not_written_error;
zend_class_entry *rapira_ce_http_content_length_exceeded_error;
zend_class_entry *rapira_ce_http_file_not_sendable_exception;

// own copy: std_object_handlers is shared engine state
static zend_object_handlers rapira_exchange_handlers;

// unit is a Rust Box; free_obj hands it back to Rust to drop
static void rapira_exchange_free(zend_object *std) {
    rapira_exchange_obj *obj = rapira_exchange_from(std);
    if (obj->unit != NULL) {
        rapira_rs_exchange_drop(obj->unit);
        obj->unit = NULL;
    }
    // no-op on IS_UNDEF (getRequest never called)
    zval_ptr_dtor(&obj->request);

    zend_object_std_dtor(std);
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, name) {
    ZEND_PARSE_PARAMETERS_NONE();
    // the plugin's root TOML section
    RETURN_STRING("http");
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, serverName) {
    ZEND_PARSE_PARAMETERS_NONE();
    RAPIRA_RETURN_THROWS_UNLESS(rapira_rs_http_server_name(return_value),
                                "serverName");
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, serverPort) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_rs_http_server_port());
}

void rapira_http_register_classes(void) {
    rapira_ce_http_form_field = register_class_Rapira_Http_FormField();
    rapira_ce_http_uploaded_file = register_class_Rapira_Http_UploadedFile();
    rapira_ce_http_multipart = register_class_Rapira_Http_Multipart();
    rapira_ce_http_request = register_class_Rapira_Http_Request();

    zend_class_entry *http_info = register_class_Rapira_Http_HttpDispatcherInfo(
        rapira_ce_dispatcher_info);
    zend_class_entry *http_exchange =
        register_class_Rapira_Http_Exchange(rapira_ce_work);
    zend_class_entry *http_dispatcher =
        register_class_Rapira_Http_HttpDispatcher(rapira_ce_dispatcher);

    rapira_ce_http_content_length_exceeded_error =
        register_class_Rapira_Http_Exception_ContentLengthExceededError(
            zend_ce_error, rapira_ce_throwable);
    rapira_ce_http_head_already_written_error =
        register_class_Rapira_Http_Exception_HeadAlreadyWrittenError(
            zend_ce_error, rapira_ce_throwable);
    rapira_ce_http_head_not_written_error =
        register_class_Rapira_Http_Exception_HeadNotWrittenError(
            zend_ce_error, rapira_ce_throwable);
    rapira_ce_http_file_not_sendable_exception =
        register_class_Rapira_Http_Exception_FileNotSendableException(
            spl_ce_RuntimeException, rapira_ce_throwable);

    rapira_ce_internal_http_dispatcher =
        register_class_Rapira_Internal_Http_Dispatcher(http_dispatcher);
    rapira_ce_internal_http_dispatcher_info =
        register_class_Rapira_Internal_Http_DispatcherInfo(http_info);
    rapira_ce_internal_http_exchange =
        register_class_Rapira_Internal_Http_Exchange(http_exchange);

    rapira_ce_internal_http_dispatcher->default_object_handlers =
        &rapira_dispatcher_handlers;

    rapira_exchange_handlers = std_object_handlers;
    rapira_exchange_handlers.clone_obj = NULL;
    rapira_exchange_handlers.offset = offsetof(rapira_exchange_obj, std);
    rapira_exchange_handlers.free_obj = rapira_exchange_free;
    rapira_ce_internal_http_exchange->create_object = rapira_object_create;
    rapira_ce_internal_http_exchange->default_object_handlers =
        &rapira_exchange_handlers;

    rapira_ce_internal_http_dispatcher_info->create_object =
        rapira_object_create;
    rapira_ce_internal_http_dispatcher_info->default_object_handlers =
        &rapira_info_handlers;
}
