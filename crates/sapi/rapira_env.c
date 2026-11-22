#include "rapira_sapi.h"

typedef struct {
    const char *name;
    size_t name_len;
    const char *value;
    size_t value_len;
} rapira_env_pair;

extern bool rapira_rs_env_get(const char *name, size_t name_len,
                              const char **value, size_t *value_len);
extern void rapira_rs_env_put(const char *name, size_t name_len,
                              const char *value, size_t value_len);
extern const rapira_env_pair *rapira_rs_env_view(size_t *len);

// PHP 8.6 added the NUL byte check to getenv() and putenv(). https://github.com/php/php-src/commit/701d8a66d327beae2b83d6c7d53711717bec14fb
#if PHP_VERSION_ID >= 80600
#define RAPIRA_PARAM_ENV Z_PARAM_PATH
#define RAPIRA_PARAM_ENV_OR_NULL Z_PARAM_PATH_OR_NULL
#else
#define RAPIRA_PARAM_ENV Z_PARAM_STRING
#define RAPIRA_PARAM_ENV_OR_NULL Z_PARAM_STRING_OR_NULL
#endif

// This SAPI has no getenv hook. Both values of local_only use the same table.
static ZEND_NAMED_FUNCTION(rapira_getenv) {
    char *name = NULL;
    size_t name_len = 0;
    bool local_only = false;

    ZEND_PARSE_PARAMETERS_START(0, 2)
    Z_PARAM_OPTIONAL
    RAPIRA_PARAM_ENV_OR_NULL(name, name_len)
    Z_PARAM_BOOL(local_only)
    ZEND_PARSE_PARAMETERS_END();
    (void)local_only;

    if (!name) {
        array_init(return_value);
        php_load_environment_variables(return_value);
        return;
    }
    const char *value = NULL;
    size_t value_len = 0;
    if (rapira_rs_env_get(name, strnlen(name, name_len), &value, &value_len)) {
        RETURN_STRINGL(value, value_len);
    }
    RETURN_FALSE;
}

// A bare name removes the variable. A value starts after the first '=' and can be empty. https://github.com/php/php-src/blob/php-8.5.11/ext/standard/basic_functions.c#L760-L797
static ZEND_NAMED_FUNCTION(rapira_putenv) {
    char *setting = NULL;
    size_t setting_len = 0;

    ZEND_PARSE_PARAMETERS_START(1, 1)
    RAPIRA_PARAM_ENV(setting, setting_len)
    ZEND_PARSE_PARAMETERS_END();

    if (setting_len == 0 || setting[0] == '=') {
        zend_argument_value_error(1, "must have a valid syntax");
        RETURN_THROWS();
    }
    setting_len = strnlen(setting, setting_len);
    const char *eq = memchr(setting, '=', setting_len);
    if (eq) {
        size_t name_len = (size_t)(eq - setting);
        rapira_rs_env_put(setting, name_len, eq + 1,
                          setting_len - name_len - 1);
    } else {
        rapira_rs_env_put(setting, setting_len, NULL, 0);
    }
    RETURN_TRUE;
}

// PHP skips malformed names during import. It stores a numeric name as an integer key. https://github.com/php/php-src/blob/php-8.5.11/main/php_variables.c#L592-L627
static void rapira_env_import(zval *array_ptr) {
    size_t len = 0;
    const rapira_env_pair *pairs = rapira_rs_env_view(&len);
    HashTable *ht = Z_ARRVAL_P(array_ptr);
    for (size_t i = 0; i < len; i++) {
        const rapira_env_pair *p = &pairs[i];
        if (p->name_len == 0 || p->name[0] == '=' ||
            memchr(p->name, ' ', p->name_len) ||
            memchr(p->name, '.', p->name_len) ||
            memchr(p->name, '[', p->name_len)) {
            continue;
        }
        zend_string *key = zend_string_init(p->name, p->name_len, 0);
        if (p->value) {
            zval value;
            ZVAL_STRINGL_FAST(&value, p->value, p->value_len);
            zend_symtable_update_ind(ht, key, &value);
        } else {
            zend_symtable_del(ht, key);
        }
        zend_string_release_ex(key, 0);
    }
}

static void rapira_env_swap(const char *name, size_t len, zif_handler handler) {
    // PHP removes disabled functions from this table. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_API.c#L3701-L3712
    zend_function *func = zend_hash_str_find_ptr(CG(function_table), name, len);
    if (func != NULL && func->type == ZEND_INTERNAL_FUNCTION) {
        func->internal_function.handler = handler;
    }
}

// Each ZTS thread copies the function table and shares the internal function records. https://github.com/php/php-src/blob/php-8.5.11/Zend/zend.c#L718-L721
void rapira_env_install(void) {
    php_import_environment_variables = rapira_env_import;
    rapira_env_swap(ZEND_STRL("getenv"), rapira_getenv);
    rapira_env_swap(ZEND_STRL("putenv"), rapira_putenv);
}
