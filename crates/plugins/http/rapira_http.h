#ifndef RAPIRA_HTTP_H
#define RAPIRA_HTTP_H

#include "../../sapi/rapira_sapi.h"

zend_string *rapira_rs_http_server_name(void);
zend_long rapira_rs_http_server_port(void);

// The class entries that rapira_http.c reads. rapira_http_register_classes
// assigns them in MINIT, after the base classes. Rust declares its own externs
// (src/php/mod.rs).
extern zend_class_entry *rapira_ce_http_multipart;

void rapira_http_register_classes(void);

#endif // RAPIRA_HTTP_H
