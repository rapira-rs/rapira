#ifndef RAPIRA_GRPC_H
#define RAPIRA_GRPC_H

#include "rapira_sapi.h"

// rust glue
extern void rapira_rs_grpc_drop(void *state);

// C data sits before zend_object std, as in rapira_sapi.h
typedef struct {
    void *state; // Box<GrpcState>, owned by Rust; NULL until receive() adopts a unit
    zval message; // memoized request message; IS_UNDEF until getMessage()
    zval context; // memoized Rapira\Grpc\Call\Context; IS_UNDEF until getContext()
    zval metadata; // memoized Rapira\Internal\Grpc\ResponseMetadata; IS_UNDEF until getResponseMetadata()
    zend_object std;
} rapira_grpc_call_obj;

typedef struct {
    void *state; // borrowed from the call; the free_obj of the call sets it to NULL
    zend_object std;
} rapira_grpc_metadata_obj;

// Class entries of the grpc stub; rapira_grpc_register_classes assigns them in MINIT, after the base classes.
extern zend_class_entry *rapira_ce_grpc_status_code;
extern zend_class_entry *rapira_ce_grpc_method_kind;
extern zend_class_entry *rapira_ce_grpc_protocol;
extern zend_class_entry *rapira_ce_grpc_status;
extern zend_class_entry *rapira_ce_grpc_metadata;
extern zend_class_entry *rapira_ce_grpc_error_detail;
extern zend_class_entry *rapira_ce_grpc_method_info;
extern zend_class_entry *rapira_ce_grpc_service_info;
extern zend_class_entry *rapira_ce_grpc_context;
extern zend_class_entry *rapira_ce_grpc_exception;
extern zend_class_entry *rapira_ce_internal_grpc_dispatcher;
extern zend_class_entry *rapira_ce_internal_grpc_dispatcher_info;
extern zend_class_entry *rapira_ce_internal_grpc_unary_call;
extern zend_class_entry *rapira_ce_internal_grpc_response_metadata;

// the register function of the grpc plugin part
void rapira_grpc_register_classes(void);

static zend_always_inline rapira_grpc_call_obj *
rapira_grpc_call_from(zend_object *obj) {
    return (rapira_grpc_call_obj *)((char *)obj -
                                    offsetof(rapira_grpc_call_obj, std));
}

static zend_always_inline rapira_grpc_metadata_obj *
rapira_grpc_metadata_from(zend_object *obj) {
    return (
        rapira_grpc_metadata_obj *)((char *)obj -
                                    offsetof(rapira_grpc_metadata_obj, std));
}

#endif // RAPIRA_GRPC_H
