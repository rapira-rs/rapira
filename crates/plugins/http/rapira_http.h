#ifndef RAPIRA_HTTP_H
#define RAPIRA_HTTP_H

#include "rapira_sapi.h"

// rust glue
extern void rapira_rs_exchange_drop(void *job);

// C data sits before zend_object std, as in rapira_sapi.h
typedef struct {
    void *job; // Box<ExchangeState> -> owned by Rust, NULLing when released
    zval request; // cached Rapira\Http\Request; IS_UNDEF until getRequest()
    zend_object std;
} rapira_exchange_obj;

// Class entries of the http stub; rapira_http_register_classes assigns them in MINIT, after the base classes.
extern zend_class_entry *rapira_ce_http_multipart;
extern zend_class_entry *rapira_ce_internal_http_dispatcher;
extern zend_class_entry *rapira_ce_internal_http_exchange;
extern zend_class_entry *rapira_ce_internal_http_dispatcher_info;
extern zend_class_entry *rapira_ce_http_head_already_written_error;
extern zend_class_entry *rapira_ce_http_head_not_written_error;
extern zend_class_entry *rapira_ce_http_content_length_exceeded_error;
extern zend_class_entry *rapira_ce_http_file_not_sendable_exception;
extern zend_class_entry *rapira_ce_http_form_field;
extern zend_class_entry *rapira_ce_http_uploaded_file;
extern zend_class_entry *rapira_ce_http_request;

// the register function of the http plugin part
void rapira_http_register_classes(void);

static zend_always_inline rapira_exchange_obj *
rapira_exchange_from(zend_object *obj) {
    // std is embedded in the enclosing struct; step back by its offset to reach the C fields
    return (rapira_exchange_obj *)((char *)obj -
                                   offsetof(rapira_exchange_obj, std));
}

#endif // RAPIRA_HTTP_H
