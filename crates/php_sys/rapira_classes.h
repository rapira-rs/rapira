#ifndef RAPIRA_CLASSES_H
#define RAPIRA_CLASSES_H

#include "wrapper.h"
#include "zend_API.h"
#include "zend_property_hooks.h"

// rust glue
extern void rapira_rs_exchange_drop(void *job);
extern void rapira_rs_dispatcher_release(void);

// Class-entry externs and object layouts live in wrapper.h, the bindgen input.

// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);

// ext_functions[] - needs const initialization
const zend_function_entry *rapira_php_functions(void);

static zend_always_inline void rapira_throw_or_backstop(const char *what) {
    if (!EG(exception)) {
        zend_throw_error(NULL, "%s failed", what);
    }
}

// https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
static zend_always_inline rapira_exchange_obj *
rapira_exchange_from(zend_object *obj) {
    // std is embedded in the enclosing struct; step back by its offset to reach the C fields
    return (rapira_exchange_obj *)((char *)obj -
                                   offsetof(rapira_exchange_obj, std));
}

static zend_always_inline rapira_dispatcher_info_obj *
rapira_dispatcher_info_from(zend_object *obj) {
    return (rapira_dispatcher_info_obj *)((char *)obj -
                                          offsetof(rapira_dispatcher_info_obj,
                                                   std));
}

#endif // RAPIRA_CLASSES_H
