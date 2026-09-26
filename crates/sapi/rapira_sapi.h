#ifndef RAPIRA_SAPI_H
#define RAPIRA_SAPI_H

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

// injected by rapira_php_build::compile
#ifndef RAPIRA_VERSION
#define RAPIRA_VERSION "0.0.0-dev"
#endif

// array_init_size, smart_str_free and zend_symtable_str_find are macro/inline-only; shims for Rust
void rapira_array_init(zval *zv, uint32_t size);
void rapira_smart_str_free(smart_str *s);
zval *rapira_symtable_str_find(HashTable *ht, const char *str, size_t len);
// ZVAL_OBJ_COPY is a macro; the shim writes the case `name` of the enum `ce` into `dst` with a new reference
void rapira_zval_enum_case(zval *dst, zend_class_entry *ce, const char *name);

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

// rust glue
extern void rapira_rs_dispatcher_release(void);

// C data sits before zend_object std; declared here so bindgen gives Rust named fields instead of a hardcoded offset: https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
typedef struct {
    zend_long pending;
    zend_long active;
    zend_object std;
} rapira_dispatcher_info_obj;

// Class entries of the base stubs; rapira_register_classes assigns them in MINIT, before the plugin parts register and before any object of these classes can exist.
// Rust binds the entries it reads as static muts (allowed_bindings.rs); the others are C-only.
extern zend_class_entry *rapira_ce_throwable;
extern zend_class_entry *rapira_ce_work;
extern zend_class_entry *rapira_ce_dispatcher;
extern zend_class_entry *rapira_ce_dispatcher_info;
extern zend_class_entry *rapira_ce_log_level;
extern zend_class_entry *rapira_ce_mode;
extern zend_class_entry *rapira_ce_closed_exception;
extern zend_class_entry *rapira_ce_timeout_exception;
extern zend_class_entry *rapira_ce_work_discarded_exception;
extern zend_class_entry *rapira_ce_no_dispatcher_error;
extern zend_class_entry *rapira_ce_not_in_worker_mode_error;
extern zend_class_entry *rapira_ce_already_finalized_error;
extern zend_class_entry *rapira_ce_tls;
extern zend_class_entry *rapira_ce_inet_address;
extern zend_class_entry *rapira_ce_unix_address;

// rapira_register_classes fills both before the plugin parts register.
// A plugin's Dispatcher class: std handlers without clone.
extern zend_object_handlers rapira_host_handlers;
// A plugin's DispatcherInfo class: the rapira_dispatcher_info_obj layout, with rapira_dispatcher_info_create.
extern zend_object_handlers rapira_info_handlers;
zend_object *rapira_dispatcher_info_create(zend_class_entry *ce);

// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);

// ext_functions[] - needs const initialization
const zend_function_entry *rapira_php_functions(void);

// The Dispatcher and DispatcherInfo method bodies; a plugin's method shells call them.
void rapira_sapi_receive(INTERNAL_FUNCTION_PARAMETERS);
void rapira_sapi_try_receive(INTERNAL_FUNCTION_PARAMETERS);
void rapira_sapi_get_info(INTERNAL_FUNCTION_PARAMETERS);
void rapira_sapi_pending_count(INTERNAL_FUNCTION_PARAMETERS);
void rapira_sapi_active_count(INTERNAL_FUNCTION_PARAMETERS);

static zend_always_inline void rapira_throw_or_backstop(const char *what) {
    if (!EG(exception)) {
        zend_throw_error(NULL, "%s failed", what);
    }
}

// https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
static zend_always_inline rapira_dispatcher_info_obj *
rapira_dispatcher_info_from(zend_object *obj) {
    return (rapira_dispatcher_info_obj *)((char *)obj -
                                          offsetof(rapira_dispatcher_info_obj,
                                                   std));
}

#endif // RAPIRA_SAPI_H
