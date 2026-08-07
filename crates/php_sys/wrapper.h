#ifndef RAPIRA_WRAPPER_H
#define RAPIRA_WRAPPER_H

// clang-format off
#include <TSRM/TSRM.h>
#include <Zend/zend.h>
#include <Zend/zend_API.h>
#include <Zend/zend_compile.h>
#include <Zend/zend_globals.h>
#include <Zend/zend_exceptions.h>
#include <Zend/zend_enum.h>
#include <Zend/zend_interfaces.h>
#include <main/php.h>
#include <ext/standard/basic_functions.h>
#include <main/SAPI.h>
#include <main/php_main.h>
#include <main/php_output.h>
#include <main/php_variables.h>
// clang-format on
#ifdef HAVE_PHP_SESSION
#include <ext/session/php_session.h>
#endif
#include <Zend/zend_observer.h>
#include <ext/spl/spl_exceptions.h>
#include <ext/standard/head.h>
#include <main/php_memory_streams.h>
#include <main/php_streams.h>

sapi_globals_struct *rapira_sg(void);
zend_executor_globals *rapira_eg(void);
php_core_globals *rapira_pg(void);
void rapira_init_call_stack(void);
void rapira_process_init(void);
void rapira_release_temporary_streams(void);
int rapira_request_activate(void);
int rapira_request_shutdown(void);
size_t rapira_ub_write(const char *str, size_t len);

// RunMode in start.rs - keep in sync
enum {
    RAPIRA_MODE_CLASSIC = 0,
    RAPIRA_MODE_WORKER_SUPERGLOBALS = 1,
    RAPIRA_MODE_WORKER_REQUEST = 2,
    RAPIRA_MODE_WORKER_REQUEST_ASYNC = 3,
};
extern int rapira_mode;

enum {
    RAPIRA_RECV_OK = 0,
    RAPIRA_RECV_TIMEOUT = 1,
    RAPIRA_RECV_EMPTY = 2,
    RAPIRA_RECV_CLOSED = 3,
};

enum {
    RAPIRA_VERB_OK = 0,
    RAPIRA_VERB_FINALIZED = 1,
    RAPIRA_VERB_DISCARDED = 2,
    RAPIRA_VERB_HEAD_WRITTEN = 3,
    RAPIRA_VERB_INTERIM = 4,
    RAPIRA_VERB_INVALID = 5,
    RAPIRA_VERB_OVERFLOW = 6,
};

#endif
