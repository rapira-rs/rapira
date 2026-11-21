#ifndef RAPIRA_HTTP_H
#define RAPIRA_HTTP_H

#include "../../sapi/rapira_sapi.h"

// The server identity from the http config. The plugin writes it once before
// the fork (src/lib.rs).
extern const char *rapira_http_server_name;
extern size_t rapira_http_server_name_len;
extern zend_long rapira_http_server_port;

// The C data comes before zend_object std, as in rapira_sapi.h.
typedef struct {
    rapira_held held; // the ExchangeState, which Rust owns
    zval request; // the cached Rapira\Http\Request, IS_UNDEF until getRequest()
    zend_object std;
} rapira_exchange_obj;
// rapira_held_of (rapira_sapi.h) subtracts handlers.offset from the object
// address to find the held word.
static_assert(offsetof(rapira_exchange_obj, held) == 0,
              "the held word must be the first field of the prefix");

// The class entries that rapira_http.c reads. rapira_http_register_classes
// assigns them in MINIT, after the base classes. Rust declares its own externs
// (src/php/mod.rs).
extern zend_class_entry *rapira_ce_http_multipart;

void rapira_http_register_classes(void);

static zend_always_inline rapira_exchange_obj *
rapira_exchange_from(zend_object *obj) {
    // std is embedded in the struct, which starts at std minus its offset.
    return (rapira_exchange_obj *)((char *)obj -
                                   offsetof(rapira_exchange_obj, std));
}

#endif // RAPIRA_HTTP_H
