#ifndef RAPIRA_HTTP_H
#define RAPIRA_HTTP_H

#include "../../sapi/rapira_sapi.h"

// rust glue
extern void rapira_rs_exchange_drop(void *unit);
extern bool rapira_rs_http_server_name(zval *return_value);
extern zend_long rapira_rs_http_server_port(void);

// C data sits before zend_object std, as in rapira_sapi.h
typedef struct {
    void *unit; // Box<ExchangeState>, owned by Rust; NULL until the receive path attaches a unit
    zval request; // cached Rapira\Http\Request; IS_UNDEF until getRequest()
    zend_object std;
} rapira_exchange_obj;

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
