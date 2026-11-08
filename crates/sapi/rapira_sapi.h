#ifndef RAPIRA_SAPI_H
#define RAPIRA_SAPI_H

// clang-format off
#include <Zend/zend_exceptions.h>
#include <Zend/zend_enum.h>
#include <Zend/zend_interfaces.h>
#include <main/php.h>
#include <ext/standard/basic_functions.h>
#include <main/php_main.h>
#include <main/php_variables.h>
// clang-format on

// ZTS comes from the main/php_config.h of the headers this build compiles against.
#ifdef ZTS
#error "rapira is NTS-only, but these PHP headers are from a thread-safe (ZTS) build. Rebuild PHP without --enable-zts, or point PHP_CONFIG at an NTS php-config."
#endif
// The http write park pauses the setitimer() execution timer (respond.rs). This option replaces that timer with a POSIX timer: https://github.com/php/php-src/blob/php-8.5.12/Zend/Zend.m4#L446-L455
#ifdef ZEND_MAX_EXECUTION_TIMERS
#error "rapira needs the setitimer() execution timer, but these PHP headers enable ZEND_MAX_EXECUTION_TIMERS. Rebuild PHP without --enable-zend-max-execution-timers."
#endif

#ifdef HAVE_PHP_SESSION
#include <ext/session/php_session.h>
#endif
#include <ext/json/php_json.h>
#include <Zend/zend_observer.h>
#include <ext/spl/spl_exceptions.h>
#include <ext/standard/head.h>

// injected by rapira_php_build::compile
#ifndef RAPIRA_VERSION
#define RAPIRA_VERSION "0.0.0-dev"
#endif

// array_init_size, smart_str_free and zend_array_is_list need C wrappers for Rust.
void rapira_array_init(zval *zv, uint32_t size);
void rapira_smart_str_free(smart_str *s);
bool rapira_array_is_list(HashTable *ht);
// ZVAL_OBJ_COPY is a macro; the shim writes the case `name` of the enum `ce` into `dst` with a new reference
void rapira_zval_enum_case(zval *dst, zend_class_entry *ce, const char *name);
// ZVAL_STRINGL is a macro; the shim writes a new non-interned string into `zv`
void rapira_zval_stringl(zval *zv, const char *s, size_t len);
// ZVAL_NEW_STR and ZVAL_EMPTY_STRING are macros; the shim writes a new string of
// len bytes into zv and returns its buffer for the caller to fill. For len 0 it
// writes the empty string and returns NULL.
char *rapira_zval_string_alloc(zval *zv, size_t len);
// zend_string_addref and zend_string_release are inline; the shims add and
// release one reference, and an interned string gets neither
void rapira_zstr_addref(zend_string *s);
void rapira_zstr_release(zend_string *s);
// ZVAL_STRINGL_FAST is a macro; the shim registers the string `val` as `name` through php_register_known_variable.
// `name` must be non-empty and non-numeric, have no NUL, ' ', '.' or '[', and not be "this" or "GLOBALS".
void rapira_register_known_stringl(const char *name, size_t name_len,
                                   const char *val, size_t val_len,
                                   zval *track_vars_array);

// Mode in rapira_config (crates/config/src/pool.rs), mapped in start.rs (start_worker) - keep in sync
enum {
    RAPIRA_MODE_CLASSIC = 0,
    RAPIRA_MODE_WORKER = 1,
    RAPIRA_MODE_DISPATCHER = 2,
};
extern int rapira_mode;

// The dispatcher surface of the plugin this worker serves. In dispatcher
// mode, worker_main (start.rs) writes these globals once before PHP runs on
// the thread.
extern zend_class_entry *rapira_plugin_ce_dispatcher;
extern zend_class_entry *rapira_plugin_ce_info;
extern zend_class_entry *rapira_plugin_ce_unit;
// the error of receive() and tryReceive() while a unit is unfinalized
extern const char *rapira_plugin_busy;
// RSHUTDOWN calls it on the PHP thread before the request strings are freed:
// the plugin waits until its IO thread holds no request string, then releases
// the strings. NULL when the plugin lends nothing.
extern void (*rapira_plugin_quiesce)(void);
// The plugin register functions that MINIT calls after the base classes, in
// array order; boot_master (start.rs) writes them once in the master.
extern void (*const *rapira_plugin_registers)(void);
extern size_t rapira_plugin_register_count;

// Return values of rapira_rs_handle_request (rapira_worker.rs);
// rapira_dispatcher.c reads them.
enum {
    RAPIRA_HANDLE_STOP = 0,
    RAPIRA_HANDLE_CONTINUE = 1,
    RAPIRA_HANDLE_RECYCLE = 2,
};

// Return values of the request shims in module.c.
enum { RAPIRA_OK = 0, RAPIRA_BAILOUT = 1, RAPIRA_THROW = 3 };

// RSHUTDOWN releases the memoized plugin Dispatcher (rapira_dispatcher.c)
void rapira_dispatcher_release(void);
// rapira_request_shutdown calls it after each request shutdown. It clears the
// memo without a dtor when a bailout in RSHUTDOWN skipped the release.
void rapira_dispatcher_forget(void);

// rust glue
// the seal of the current CGI reply (rapira_worker.rs)
extern void rapira_rs_finish_response(void);

// The unit behind a Work object: the first field of the C prefix of every class
// that receive() hands out. Rust owns the two words (HeldPtr in work.rs); C
// passes the address, copies or zeroes the struct as a whole, and never reads
// the words. A zeroed rapira_held holds no unit. zend_object_alloc zeroes the
// prefix, so a fresh object holds no unit.
typedef struct {
    void *words[2];
} rapira_held;

// The held word of a unit object: the prefix starts handlers.offset bytes
// before the object (rapira_object_create), and the held word is its first
// field.
static zend_always_inline rapira_held *rapira_held_of(zend_object *obj) {
    return (rapira_held *)((char *)obj - obj->handlers->offset);
}

// free_obj of a unit class passes the held word of the object: Rust reclaims
// the unit (work.rs)
extern void rapira_rs_held_drop(rapira_held *held);

// The counts of a DispatcherInfo object sit before zend_object std:
// https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
typedef struct {
    zend_long pending;
    zend_long active;
    zend_object std;
} rapira_dispatcher_info_obj;

// Class entries of the base stubs; rapira_register_classes assigns them in MINIT, before the plugin classes register and before any object of these classes can exist.
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

// rapira_register_classes fills both before the plugin classes register.
// A plugin's Dispatcher class: std handlers without clone.
extern zend_object_handlers rapira_dispatcher_handlers;
// A plugin's DispatcherInfo class: the rapira_dispatcher_info_obj layout.
extern zend_object_handlers rapira_info_handlers;
// create_object for every class with a C prefix; the handler offset gives the
// prefix size.
zend_object *rapira_object_create(zend_class_entry *ce);

// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);
// Checks the Rapira\InetAddress|Rapira\UnixAddress union that arginfo cannot
// enforce: internal arg types are debug-only. Returns false with a TypeError
// pending.
bool rapira_address_arg(zval *zv, uint32_t num);
// writes the string property name, or null when value is NULL
static zend_always_inline void
rapira_prop_str_or_null(zend_class_entry *ce, zend_object *obj,
                        const char *name, size_t name_len, zend_string *value) {
    if (value) {
        zend_update_property_str(ce, obj, name, name_len, value);
    } else {
        zend_update_property_null(ce, obj, name, name_len);
    }
}
// called from start.rs after php_module_startup: the post readers that hand
// PHP the buffered body (module.c)
void rapira_register_post_reader(void);

// The Dispatcher, DispatcherInfo and Work methods that every plugin shares, and
// the private constructors of its host-created classes. The plugin stubs name
// them with @implementation-alias, so the plugin arginfo points at them
// directly.
ZEND_FUNCTION(rapira_sapi_receive);
ZEND_FUNCTION(rapira_sapi_try_receive);
ZEND_FUNCTION(rapira_sapi_get_info);
ZEND_FUNCTION(rapira_sapi_pending_count);
ZEND_FUNCTION(rapira_sapi_active_count);
ZEND_FUNCTION(rapira_sapi_is_finalized);
ZEND_FUNCTION(rapira_sapi_is_cancelled);
ZEND_FUNCTION(rapira_sapi_dispatcher_construct);
ZEND_FUNCTION(rapira_sapi_host_created);

// Returns from the internal function when `call` reports false. Rust throws
// before it reports false; the backstop covers a missing throw.
#define RAPIRA_RETURN_THROWS_UNLESS(call, what)                                \
    do {                                                                       \
        if (!(call)) {                                                         \
            if (!EG(exception)) {                                              \
                zend_throw_error(NULL, "%s failed", what);                     \
            }                                                                  \
            RETURN_THROWS();                                                   \
        }                                                                      \
    } while (0)

// https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
static zend_always_inline rapira_dispatcher_info_obj *
rapira_dispatcher_info_from(zend_object *obj) {
    return (rapira_dispatcher_info_obj *)((char *)obj -
                                          offsetof(rapira_dispatcher_info_obj,
                                                   std));
}

#endif // RAPIRA_SAPI_H
