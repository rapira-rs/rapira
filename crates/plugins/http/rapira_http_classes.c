#include "rapira_http.h"
#include "rapira_http_arginfo.h"

zend_class_entry *rapira_ce_http_multipart;
zend_class_entry *rapira_ce_http_form_field;
zend_class_entry *rapira_ce_http_uploaded_file;
zend_class_entry *rapira_ce_http_request;
zend_class_entry *rapira_ce_http_head_already_written_error;
zend_class_entry *rapira_ce_http_head_not_written_error;
zend_class_entry *rapira_ce_http_content_length_exceeded_error;
zend_class_entry *rapira_ce_http_file_not_sendable_exception;
// The unit class. ExchangeState::class returns it (src/php/mod.rs).
zend_class_entry *rapira_ce_http_exchange;

// The plugin record. Plugin::dispatcher() returns its address (src/lib.rs).
rapira_plugin rapira_http_plugin = {
    .busy = "this worker holds one unit at a time, and its "
            "Rapira\\Http\\Exchange is unfinalized; finalize it first",
};

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, serverName) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_STR_COPY(rapira_rs_http_server_name());
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

    rapira_http_plugin.dispatcher =
        register_class_Rapira_Internal_Http_Dispatcher(http_dispatcher);
    rapira_http_plugin.info =
        register_class_Rapira_Internal_Http_DispatcherInfo(http_info);
    rapira_ce_http_exchange =
        register_class_Rapira_Internal_Http_Exchange(http_exchange);
    rapira_plugin_wire(&rapira_http_plugin);
    rapira_unit_wire(rapira_ce_http_exchange);
}
