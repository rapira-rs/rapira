#ifndef RAPIRA_HTTP_H
#define RAPIRA_HTTP_H

#include "../../sapi/rapira_sapi.h"

// rust glue
extern bool rapira_rs_http_server_name(zval *return_value);
extern zend_long rapira_rs_http_server_port(void);

// C data sits before zend_object std, as in rapira_sapi.h
typedef struct {
    rapira_held held; // the ExchangeState, owned by Rust
    zval request; // cached Rapira\Http\Request; IS_UNDEF until getRequest()
    zend_object std;
} rapira_exchange_obj;
// HeldPtr::of (work.rs) steps back handlers.offset bytes to the held word
ZEND_STATIC_ASSERT(offsetof(rapira_exchange_obj, held) == 0,
                   "the held word must be the first field of the prefix");

// Class entries that rapira_http.c reads; rapira_http_register_classes assigns
// them in MINIT, after the base classes. Rust declares its own externs
// (src/php/mod.rs).
extern zend_class_entry *rapira_ce_http_multipart;

void rapira_http_register_classes(void);

static zend_always_inline rapira_exchange_obj *
rapira_exchange_from(zend_object *obj) {
    // std is embedded in the enclosing struct; step back by its offset to reach the C fields
    return (rapira_exchange_obj *)((char *)obj -
                                   offsetof(rapira_exchange_obj, std));
}

#endif // RAPIRA_HTTP_H
