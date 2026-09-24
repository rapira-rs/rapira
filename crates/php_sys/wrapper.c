#include "wrapper.h"

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

void rapira_init_call_stack(void) {
#ifdef ZEND_CHECK_STACK_LIMIT
    zend_call_stack_init();
#endif
}
