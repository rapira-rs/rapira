#ifndef RAPIRA_GRPC_H
#define RAPIRA_GRPC_H

#include "../../sapi/rapira_sapi.h"

// C data sits before zend_object std, as in rapira_sapi.h
typedef struct {
    rapira_held held; // the GrpcState, owned by Rust
    zval message; // memoized request message; IS_UNDEF until getMessage()
    zval context; // memoized Rapira\Grpc\Call\Context; IS_UNDEF until getContext()
    zval metadata; // memoized Rapira\Internal\Grpc\ResponseMetadata; IS_UNDEF until getResponseMetadata()
    zend_object std;
} rapira_grpc_call_obj;
// HeldPtr::of (work.rs) and rapira_held_of (rapira_sapi.h) step back
// handlers.offset bytes to the held word
static_assert(offsetof(rapira_grpc_call_obj, held) == 0,
              "the held word must be the first field of the prefix");

typedef struct {
    rapira_held held; // a copy of the held word of the call; the free_obj of the call zeroes it
    zend_object std;
} rapira_grpc_metadata_obj;

// Class entries that rapira_grpc.c reads; rapira_grpc_register_classes assigns
// them in MINIT, after the base classes. Rust declares its own externs
// (src/php/mod.rs).
extern zend_class_entry *rapira_ce_grpc_status_code;
extern zend_class_entry *rapira_ce_grpc_method_kind;
extern zend_class_entry *rapira_ce_grpc_protocol;
extern zend_class_entry *rapira_ce_grpc_status;
extern zend_class_entry *rapira_ce_grpc_metadata;
extern zend_class_entry *rapira_ce_grpc_exception;
extern zend_class_entry *rapira_ce_internal_grpc_response_metadata;

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
