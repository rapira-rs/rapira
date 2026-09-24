#include "rapira_classes.h"
#include "ext/spl/spl_exceptions.h"
#include "rapira_arginfo.h"
#include "rapira_exception_arginfo.h"
#include "rapira_grpc_arginfo.h"
#include "rapira_http_arginfo.h"
#include "zend_API.h"
#include "zend_exceptions.h"
#include "zend_object_handlers.h"
#include "zend_objects.h"
#include "zend_objects_API.h"
#include "zend_portability.h"
#include "zend_property_hooks.h"
#include "zend_types.h"

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

zend_class_entry *rapira_ce_grpc_status_code;
zend_class_entry *rapira_ce_grpc_method_kind;
zend_class_entry *rapira_ce_grpc_protocol;
zend_class_entry *rapira_ce_grpc_status;
zend_class_entry *rapira_ce_grpc_metadata;
zend_class_entry *rapira_ce_grpc_error_detail;
zend_class_entry *rapira_ce_grpc_method_info;
zend_class_entry *rapira_ce_grpc_service_info;
zend_class_entry *rapira_ce_grpc_context;
zend_class_entry *rapira_ce_grpc_exception;
zend_class_entry *rapira_ce_internal_grpc_dispatcher;
zend_class_entry *rapira_ce_internal_grpc_dispatcher_info;
zend_class_entry *rapira_ce_internal_grpc_unary_call;
zend_class_entry *rapira_ce_internal_grpc_response_metadata;

const zend_function_entry *rapira_php_functions(void) { return ext_functions; }

// own copies: std_object_handlers is shared engine state
static zend_object_handlers rapira_host_handlers;
static zend_object_handlers rapira_exchange_handlers;
static zend_object_handlers rapira_info_handlers;
static zend_object_handlers rapira_grpc_call_handlers;
static zend_object_handlers rapira_grpc_metadata_handlers;

static zend_object *rapira_exchange_create(zend_class_entry *ce) {
    rapira_exchange_obj *obj = zend_object_alloc(sizeof(*obj), ce);
    obj->job = NULL;
    ZVAL_UNDEF(&obj->request);
    zend_object_std_init(&obj->std, ce);
    object_properties_init(&obj->std, ce);
    return &obj->std;
}

// job is a Rust Box; free_obj hands it back to Rust to drop
static void rapira_exchange_free(zend_object *std) {
    rapira_exchange_obj *obj = rapira_exchange_from(std);
    if (obj->job != NULL) {
        rapira_rs_exchange_drop(obj->job);
        obj->job = NULL;
    }
    // no-op on IS_UNDEF (getRequest never called)
    zval_ptr_dtor(&obj->request);

    zend_object_std_dtor(std);
}

static zend_object *rapira_grpc_call_create(zend_class_entry *ce) {
    rapira_grpc_call_obj *obj = zend_object_alloc(sizeof(*obj), ce);
    obj->state = NULL;
    ZVAL_UNDEF(&obj->context);
    ZVAL_UNDEF(&obj->metadata);
    zend_object_std_init(&obj->std, ce);
    object_properties_init(&obj->std, ce);
    return &obj->std;
}

// state is a Rust Box; free_obj hands it back to Rust to drop. receive()
// allocates the object before the pull, so state is NULL when no call came.
static void rapira_grpc_call_free(zend_object *std) {
    rapira_grpc_call_obj *obj = rapira_grpc_call_from(std);
    // a kept ResponseMetadata object must not reach the freed state
    if (Z_TYPE(obj->metadata) == IS_OBJECT) {
        rapira_grpc_metadata_from(Z_OBJ(obj->metadata))->state = NULL;
    }
    if (obj->state != NULL) {
        rapira_rs_grpc_drop(obj->state);
        obj->state = NULL;
    }
    // no-op on IS_UNDEF
    zval_ptr_dtor(&obj->context);
    zval_ptr_dtor(&obj->metadata);

    zend_object_std_dtor(std);
}

static zend_object *rapira_grpc_metadata_create(zend_class_entry *ce) {
    rapira_grpc_metadata_obj *obj = zend_object_alloc(sizeof(*obj), ce);
    obj->state = NULL;
    zend_object_std_init(&obj->std, ce);
    object_properties_init(&obj->std, ce);
    return &obj->std;
}

static zend_object *rapira_dispatcher_info_create(zend_class_entry *ce) {
    rapira_dispatcher_info_obj *obj = zend_object_alloc(sizeof(*obj), ce);
    obj->pending = 0;
    obj->active = 0;
    zend_object_std_init(&obj->std, ce);
    object_properties_init(&obj->std, ce);
    return &obj->std;
}

void rapira_register_classes(void) {
    zend_class_entry *throwable =
        register_class_Rapira_Exception_RapiraThrowable(zend_ce_throwable);

    rapira_ce_closed_exception =
        register_class_Rapira_Exception_ClosedException(spl_ce_RuntimeException,
                                                        throwable);
    rapira_ce_timeout_exception =
        register_class_Rapira_Exception_TimeoutException(
            spl_ce_RuntimeException, throwable);
    rapira_ce_work_discarded_exception =
        register_class_Rapira_Exception_WorkDiscardedException(
            spl_ce_RuntimeException, throwable);
    rapira_ce_no_dispatcher_error =
        register_class_Rapira_Exception_NoDispatcherError(zend_ce_error,
                                                          throwable);
    rapira_ce_not_in_worker_mode_error =
        register_class_Rapira_Exception_NotInWorkerModeError(zend_ce_error,
                                                             throwable);
    rapira_ce_already_finalized_error =
        register_class_Rapira_Exception_AlreadyFinalizedError(zend_ce_error,
                                                              throwable);

    rapira_ce_log_level = register_class_Rapira_LogLevel();
    rapira_ce_mode = register_class_Rapira_Mode();
    zend_class_entry *work = register_class_Rapira_Work();
    zend_class_entry *dispatcher_info = register_class_Rapira_DispatcherInfo();
    zend_class_entry *dispatcher = register_class_Rapira_Dispatcher();

    rapira_ce_inet_address = register_class_Rapira_InetAddress();
    rapira_ce_unix_address = register_class_Rapira_UnixAddress();
    rapira_ce_tls = register_class_Rapira_Tls();
    rapira_ce_http_form_field = register_class_Rapira_Http_FormField();
    rapira_ce_http_uploaded_file = register_class_Rapira_Http_UploadedFile();
    rapira_ce_http_multipart = register_class_Rapira_Http_Multipart();
    rapira_ce_http_request = register_class_Rapira_Http_Request();

    zend_class_entry *http_info =
        register_class_Rapira_Http_HttpDispatcherInfo(dispatcher_info);
    zend_class_entry *http_exchange = register_class_Rapira_Http_Exchange(work);
    zend_class_entry *http_dispatcher =
        register_class_Rapira_Http_HttpDispatcher(dispatcher);

    rapira_ce_http_content_length_exceeded_error =
        register_class_Rapira_Http_Exception_ContentLengthExceededError(
            zend_ce_error, throwable);
    rapira_ce_http_head_already_written_error =
        register_class_Rapira_Http_Exception_HeadAlreadyWrittenError(
            zend_ce_error, throwable);
    rapira_ce_http_head_not_written_error =
        register_class_Rapira_Http_Exception_HeadNotWrittenError(zend_ce_error,
                                                                 throwable);
    rapira_ce_http_file_not_sendable_exception =
        register_class_Rapira_Http_Exception_FileNotSendableException(
            spl_ce_RuntimeException, throwable);

    rapira_ce_internal_http_dispatcher =
        register_class_Rapira_Internal_Http_Dispatcher(http_dispatcher);
    rapira_ce_internal_http_dispatcher_info =
        register_class_Rapira_Internal_Http_DispatcherInfo(http_info);
    rapira_ce_internal_http_exchange =
        register_class_Rapira_Internal_Http_Exchange(http_exchange);

    rapira_ce_grpc_status_code = register_class_Rapira_Grpc_StatusCode();
    rapira_ce_grpc_method_kind = register_class_Rapira_Grpc_MethodKind();
    rapira_ce_grpc_protocol = register_class_Rapira_Grpc_Call_Protocol();
    rapira_ce_grpc_error_detail = register_class_Rapira_Grpc_ErrorDetail();
    rapira_ce_grpc_status = register_class_Rapira_Grpc_Status();
    rapira_ce_grpc_metadata = register_class_Rapira_Grpc_Metadata(
        zend_ce_countable, zend_ce_aggregate);
    rapira_ce_grpc_method_info = register_class_Rapira_Grpc_MethodInfo();
    rapira_ce_grpc_service_info = register_class_Rapira_Grpc_ServiceInfo();
    rapira_ce_grpc_context = register_class_Rapira_Grpc_Call_Context();

    zend_class_entry *grpc_call = register_class_Rapira_Grpc_Call(work);
    zend_class_entry *grpc_responder =
        register_class_Rapira_Grpc_Responder(work);
    zend_class_entry *grpc_unary_request =
        register_class_Rapira_Grpc_UnaryRequest(grpc_call);
    zend_class_entry *grpc_streaming_request =
        register_class_Rapira_Grpc_StreamingRequest(grpc_call);
    zend_class_entry *grpc_unary_responder =
        register_class_Rapira_Grpc_UnaryResponder(grpc_responder);
    zend_class_entry *grpc_streaming_responder =
        register_class_Rapira_Grpc_StreamingResponder(grpc_responder);
    zend_class_entry *grpc_unary_call = register_class_Rapira_Grpc_UnaryCall(
        grpc_unary_request, grpc_unary_responder);
    register_class_Rapira_Grpc_ServerStreamingCall(grpc_unary_request,
                                                   grpc_streaming_responder);
    register_class_Rapira_Grpc_ClientStreamingCall(grpc_streaming_request,
                                                   grpc_unary_responder);
    register_class_Rapira_Grpc_BidiStreamingCall(grpc_streaming_request,
                                                 grpc_streaming_responder);
    register_class_Rapira_Grpc_Call_MessageStream(zend_ce_aggregate);
    zend_class_entry *grpc_response_metadata =
        register_class_Rapira_Grpc_Responder_ResponseMetadata();
    zend_class_entry *grpc_info =
        register_class_Rapira_Grpc_GrpcDispatcherInfo(dispatcher_info);
    zend_class_entry *grpc_dispatcher =
        register_class_Rapira_Grpc_GrpcDispatcher(dispatcher);

    rapira_ce_grpc_exception =
        register_class_Rapira_Grpc_Exception_GrpcException(
            spl_ce_RuntimeException, throwable);
    register_class_Rapira_Grpc_Exception_HeadersAlreadyCommittedError(
        zend_ce_error, throwable);

    rapira_ce_internal_grpc_dispatcher =
        register_class_Rapira_Internal_Grpc_Dispatcher(grpc_dispatcher);
    rapira_ce_internal_grpc_dispatcher_info =
        register_class_Rapira_Internal_Grpc_DispatcherInfo(grpc_info);
    rapira_ce_internal_grpc_unary_call =
        register_class_Rapira_Internal_Grpc_UnaryCall(grpc_unary_call);
    rapira_ce_internal_grpc_response_metadata =
        register_class_Rapira_Internal_Grpc_ResponseMetadata(
            grpc_response_metadata);

    // clone_obj = NULL: engine throws on clone (Zend/zend_vm_def.h:6050-6056)
    memcpy(&rapira_host_handlers, &std_object_handlers,
           sizeof(rapira_host_handlers));
    rapira_host_handlers.clone_obj = NULL;
    rapira_ce_internal_http_dispatcher->default_object_handlers =
        &rapira_host_handlers;
    rapira_ce_internal_grpc_dispatcher->default_object_handlers =
        &rapira_host_handlers;

    memcpy(&rapira_exchange_handlers, &std_object_handlers,
           sizeof(rapira_exchange_handlers));
    rapira_exchange_handlers.clone_obj = NULL;
    rapira_exchange_handlers.offset = offsetof(rapira_exchange_obj, std);
    rapira_exchange_handlers.free_obj = rapira_exchange_free;
    rapira_ce_internal_http_exchange->create_object = rapira_exchange_create;
    rapira_ce_internal_http_exchange->default_object_handlers =
        &rapira_exchange_handlers;

    memcpy(&rapira_info_handlers, &std_object_handlers,
           sizeof(rapira_info_handlers));
    rapira_info_handlers.clone_obj = NULL;
    rapira_info_handlers.offset = offsetof(rapira_dispatcher_info_obj, std);
    rapira_ce_internal_http_dispatcher_info->create_object =
        rapira_dispatcher_info_create;
    rapira_ce_internal_http_dispatcher_info->default_object_handlers =
        &rapira_info_handlers;
    rapira_ce_internal_grpc_dispatcher_info->create_object =
        rapira_dispatcher_info_create;
    rapira_ce_internal_grpc_dispatcher_info->default_object_handlers =
        &rapira_info_handlers;

    memcpy(&rapira_grpc_call_handlers, &std_object_handlers,
           sizeof(rapira_grpc_call_handlers));
    rapira_grpc_call_handlers.clone_obj = NULL;
    rapira_grpc_call_handlers.offset = offsetof(rapira_grpc_call_obj, std);
    rapira_grpc_call_handlers.free_obj = rapira_grpc_call_free;
    rapira_ce_internal_grpc_unary_call->create_object = rapira_grpc_call_create;
    rapira_ce_internal_grpc_unary_call->default_object_handlers =
        &rapira_grpc_call_handlers;

    // the standard free_obj: the metadata object owns nothing
    memcpy(&rapira_grpc_metadata_handlers, &std_object_handlers,
           sizeof(rapira_grpc_metadata_handlers));
    rapira_grpc_metadata_handlers.clone_obj = NULL;
    rapira_grpc_metadata_handlers.offset =
        offsetof(rapira_grpc_metadata_obj, std);
    rapira_ce_internal_grpc_response_metadata->create_object =
        rapira_grpc_metadata_create;
    rapira_ce_internal_grpc_response_metadata->default_object_handlers =
        &rapira_grpc_metadata_handlers;
}
