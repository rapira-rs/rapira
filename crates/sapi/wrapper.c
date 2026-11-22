#include "rapira_sapi.h"

#include <Zend/zend_smart_str.h>

sapi_globals_struct *rapira_sg(void) {
    return (sapi_globals_struct *)((char *)&SG(server_context) -
                                   offsetof(sapi_globals_struct, server_context));
}

zend_executor_globals *rapira_eg(void) {
    return (zend_executor_globals *)((char *)&EG(exception) -
                                     offsetof(zend_executor_globals, exception));
}

php_core_globals *rapira_pg(void) {
    return (php_core_globals *)((char *)&PG(last_error_message) -
                                offsetof(php_core_globals, last_error_message));
}

zend_compiler_globals *rapira_cg(void) {
    return (zend_compiler_globals *)((char *)&CG(unclean_shutdown) -
                                     offsetof(zend_compiler_globals, unclean_shutdown));
}

void rapira_array_init(zval *zv, uint32_t size) {
    array_init_size(zv, size);
}

void rapira_smart_str_free(smart_str *s) {
    smart_str_free(s);
}

bool rapira_array_is_list(HashTable *ht) {
    return zend_array_is_list(ht);
}

void rapira_zval_enum_case(zval *dst, zend_class_entry *ce, const char *name) {
    ZVAL_OBJ_COPY(dst, zend_enum_get_case_cstr(ce, name));
}

void rapira_zval_stringl(zval *zv, const char *s, size_t len) {
    ZVAL_STRINGL(zv, s, len);
}

char *rapira_zval_string_alloc(zval *zv, size_t len) {
    if (len == 0) {
        ZVAL_EMPTY_STRING(zv);
        return NULL;
    }
    zend_string *s = zend_string_alloc(len, false);
    ZSTR_VAL(s)[len] = '\0';
    ZVAL_NEW_STR(zv, s);
    return ZSTR_VAL(s);
}

void rapira_register_known_stringl(const char *name, size_t name_len,
                                   const char *val, size_t val_len,
                                   zval *track_vars_array) {
    zval value;
    ZVAL_STRINGL_FAST(&value, val, val_len);
    php_register_known_variable(name, name_len, &value, track_vars_array);
}

void rapira_hash_update_stringl(HashTable *ht, zend_string *key,
                                const char *val, size_t val_len) {
    zval value;
    ZVAL_STRINGL_FAST(&value, val, val_len);
    zend_hash_update(ht, key, &value);
}

void rapira_zstr_addref(zend_string *s) { zend_string_addref(s); }

void rapira_zstr_release(zend_string *s) { zend_string_release(s); }

// The engine owns this immutable allocation. PHP can keep it in extension caches until module shutdown. https://github.com/php/php-src/blob/php-8.6.0/ext/pcre/php_pcre.c#L847-L863
zend_string *rapira_permanent_string(const char *s, size_t len) {
    zend_string *str = zend_string_init(s, len, true);
    zend_string_hash_val(str);
    GC_ADD_FLAGS(str, IS_STR_INTERNED | IS_STR_PERMANENT);
    return str;
}

void rapira_free_permanent_string(zend_string *s) { pefree(s, true); }
