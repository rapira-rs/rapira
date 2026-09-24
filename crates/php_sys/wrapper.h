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

// ZTS comes from the main/php_config.h of the headers this build compiles against.
#ifdef ZTS
#error "rapira is NTS-only, but these PHP headers are from a thread-safe (ZTS) build. Rebuild PHP without --enable-zts, or point PHP_CONFIG at an NTS php-config."
#endif

#ifdef HAVE_PHP_SESSION
#include <ext/session/php_session.h>
#endif
#include <ext/json/php_json.h>
#include <Zend/zend_observer.h>
#include <ext/spl/spl_exceptions.h>
#include <ext/standard/head.h>
#include <main/php_memory_streams.h>
#include <main/php_streams.h>

// injected by build.rs
#ifndef RAPIRA_VERSION
#define RAPIRA_VERSION "0.0.0-dev"
#endif

// array_init_size and smart_str_free are macro/inline-only; shims for Rust
void rapira_array_init(zval *zv, uint32_t size);
void rapira_smart_str_free(smart_str *s);

// Mode in types.rs, mapped in start.rs (start_worker) - keep in sync
enum {
    RAPIRA_MODE_CLASSIC = 0,
    RAPIRA_MODE_WORKER = 1,
    RAPIRA_MODE_DISPATCHER = 2,
};
extern int rapira_mode;

// HandleAction in rapira_worker.rs - keep in sync
enum {
    RAPIRA_HANDLE_STOP = 0,
    RAPIRA_HANDLE_CONTINUE = 1,
    RAPIRA_HANDLE_RECYCLE = 2,
};

// C data sits before zend_object std; declared here so bindgen gives Rust named fields instead of a hardcoded offset: https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
typedef struct {
    void *job; // Box<ExchangeState> -> owned by Rust, NULLing when released
    zval request; // cached Rapira\Http\Request; IS_UNDEF until getRequest()
    zend_object std;
} rapira_exchange_obj;

typedef struct {
    zend_long pending;
    zend_long active;
    zend_object std;
} rapira_dispatcher_info_obj;

// Class entries for the three stubs; rapira_register_classes assigns them in MINIT, before any object of these classes can exist.
// Rust binds them as static muts, except the C-only log_level, mode and not_in_worker_mode_error entries.
extern zend_class_entry *rapira_ce_log_level;
extern zend_class_entry *rapira_ce_mode;
extern zend_class_entry *rapira_ce_closed_exception;
extern zend_class_entry *rapira_ce_timeout_exception;
extern zend_class_entry *rapira_ce_work_discarded_exception;
extern zend_class_entry *rapira_ce_no_dispatcher_error;
extern zend_class_entry *rapira_ce_not_in_worker_mode_error;
extern zend_class_entry *rapira_ce_already_finalized_error;
extern zend_class_entry *rapira_ce_tls;
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
extern zend_class_entry *rapira_ce_http_form_field;
extern zend_class_entry *rapira_ce_http_uploaded_file;
extern zend_class_entry *rapira_ce_http_request;

#endif
