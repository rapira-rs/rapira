#include "rapira_sapi.h"

#include <Zend/zend_smart_str.h>

sapi_globals_struct *rapira_sg(void) {
    return &sapi_globals;
}

zend_executor_globals *rapira_eg(void) {
    return &executor_globals;
}

zend_compiler_globals *rapira_cg(void) {
    return &compiler_globals;
}

php_core_globals *rapira_pg(void) {
    return &core_globals;
}

void rapira_array_init(zval *zv, uint32_t size) {
    array_init_size(zv, size);
}

void rapira_smart_str_free(smart_str *s) {
    smart_str_free(s);
}

zval *rapira_symtable_str_find(HashTable *ht, const char *str, size_t len) {
    return zend_symtable_str_find(ht, str, len);
}

void rapira_zval_enum_case(zval *dst, zend_class_entry *ce, const char *name) {
    ZVAL_OBJ_COPY(dst, zend_enum_get_case_cstr(ce, name));
}

void rapira_zval_stringl(zval *zv, const char *s, size_t len) {
    ZVAL_STRINGL(zv, s, len);
}

void rapira_register_known_stringl(const char *name, size_t name_len,
                                   const char *val, size_t val_len,
                                   zval *track_vars_array) {
    zval value;
    ZVAL_STRINGL_FAST(&value, val, val_len);
    php_register_known_variable(name, name_len, &value, track_vars_array);
}

void rapira_init_call_stack(void) {
#ifdef ZEND_CHECK_STACK_LIMIT
    zend_call_stack_init();
#endif
}
