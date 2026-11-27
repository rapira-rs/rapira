#ifndef RAPIRA_GRPC_H
#define RAPIRA_GRPC_H

#include "../../sapi/rapira_sapi.h"

// Class entries that rapira_grpc.c reads; rapira_grpc_register_classes assigns
// them in MINIT, after the base classes. Rust declares its own externs
// (src/php/mod.rs).
extern zend_class_entry *rapira_ce_grpc_status_code;
extern zend_class_entry *rapira_ce_grpc_method_kind;
extern zend_class_entry *rapira_ce_grpc_protocol;
extern zend_class_entry *rapira_ce_grpc_status;
extern zend_class_entry *rapira_ce_grpc_metadata;
extern zend_class_entry *rapira_ce_grpc_exception;

void rapira_grpc_register_classes(void);

#endif // RAPIRA_GRPC_H
