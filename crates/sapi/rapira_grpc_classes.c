#include "rapira_grpc.h"
#include "ext/spl/spl_exceptions.h"
#include "rapira_grpc_arginfo.h"
#include "zend_API.h"
#include "zend_exceptions.h"
#include "zend_object_handlers.h"
#include "zend_objects.h"
#include "zend_objects_API.h"
#include "zend_property_hooks.h"
#include "zend_types.h"

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

// own copies: std_object_handlers is shared engine state
static zend_object_handlers rapira_grpc_call_handlers;
static zend_object_handlers rapira_grpc_metadata_handlers;

static zend_object *rapira_grpc_call_create(zend_class_entry *ce) {
    rapira_grpc_call_obj *obj = zend_object_alloc(sizeof(*obj), ce);
    obj->state = NULL;
    ZVAL_UNDEF(&obj->message);
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
    zval_ptr_dtor(&obj->message);
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

void rapira_grpc_register_classes(void) {
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

    zend_class_entry *grpc_call =
        register_class_Rapira_Grpc_Call(rapira_ce_work);
    zend_class_entry *grpc_responder =
        register_class_Rapira_Grpc_Responder(rapira_ce_work);
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
    zend_class_entry *grpc_info = register_class_Rapira_Grpc_GrpcDispatcherInfo(
        rapira_ce_dispatcher_info);
    zend_class_entry *grpc_dispatcher =
        register_class_Rapira_Grpc_GrpcDispatcher(rapira_ce_dispatcher);

    rapira_ce_grpc_exception =
        register_class_Rapira_Grpc_Exception_GrpcException(
            spl_ce_RuntimeException, rapira_ce_throwable);
    register_class_Rapira_Grpc_Exception_HeadersAlreadyCommittedError(
        zend_ce_error, rapira_ce_throwable);

    rapira_ce_internal_grpc_dispatcher =
        register_class_Rapira_Internal_Grpc_Dispatcher(grpc_dispatcher);
    rapira_ce_internal_grpc_dispatcher_info =
        register_class_Rapira_Internal_Grpc_DispatcherInfo(grpc_info);
    rapira_ce_internal_grpc_unary_call =
        register_class_Rapira_Internal_Grpc_UnaryCall(grpc_unary_call);
    rapira_ce_internal_grpc_response_metadata =
        register_class_Rapira_Internal_Grpc_ResponseMetadata(
            grpc_response_metadata);

    rapira_ce_internal_grpc_dispatcher->default_object_handlers =
        &rapira_host_handlers;

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
