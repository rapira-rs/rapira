#ifndef RAPIRA_CLASSES_H
#define RAPIRA_CLASSES_H

#include "wrapper.h"
#include "zend_API.h"
#include "zend_long.h"
#include "zend_property_hooks.h"

// rapira.stub.php
// log
extern zend_class_entry *rapira_ce_log_level;
extern zend_class_entry *rapira_ce_work;
extern zend_class_entry *rapira_ce_dispatcher_info;
extern zend_class_entry *rapira_ce_dispatcher;

// exceptions

extern zend_class_entry *rapira_ce_closed_exception;
extern zend_class_entry *rapira_ce_timeout_exception;
extern zend_class_entry *rapira_ce_work_discarded_exception;
extern zend_class_entry *rapira_ce_not_in_worker_mode_error;
extern zend_class_entry *rapira_ce_already_finalized_error;

// http
extern zend_class_entry *rapira_ce_http_tls;
extern zend_class_entry *rapira_ce_http_multipart;
extern zend_class_entry *rapira_ce_internal_http_dispatcher;
extern zend_class_entry *rapira_ce_inet_address;
extern zend_class_entry *rapira_ce_unix_address;

extern zend_class_entry *rapira_ce_internal_http_exchange;
extern zend_class_entry *rapira_ce_internal_http_dispatcher_info;
extern zend_class_entry *rapira_ce_http_head_already_written_error;
extern zend_class_entry *rapira_ce_http_head_not_written_error;
extern zend_class_entry *rapira_ce_http_content_length_exceeded_error;
extern zend_class_entry *rapira_ce_http_file_not_sendable_exception;

// types in rapira.stub.php
// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);
// aka drop
void rapira_dispatcher_release(void);
void rapira_receive_budget_reset(void);

// ext_functions[] - needs const initialization
const zend_function_entry *rapira_php_functions(void);

typedef struct {
    void *job; // Box<ExchangeState> -> owned by Rustttt, NULLing when released
    zend_object std;
} rapira_exchange_obj;

typedef struct {
    zend_long pending;
    zend_long active;
    zend_object std;
} rapira_dispatcher_info_obj;

#endif // RAPIRA_CLASSES_H
