#include "rapira_http.h"

// Rust glue. Verbs throw in Rust and return false with the exception pending.
extern bool rapira_rs_exchange_build_request(const rapira_held *held,
                                             zval *memo);
extern bool rapira_rs_exchange_write_head(const rapira_held *held,
                                          int64_t status, HashTable *headers);
extern bool rapira_rs_exchange_write_body(const rapira_held *held,
                                          zend_string *content, bool eos);
extern bool rapira_rs_exchange_flush(const rapira_held *held);
extern bool rapira_rs_exchange_send_file(const rapira_held *held,
                                         const char *path, size_t path_len,
                                         const char *cwd, int64_t offset,
                                         int64_t length, bool length_is_null,
                                         bool eos);
extern bool rapira_rs_exchange_write_trailers(const rapira_held *held,
                                              HashTable *trailers);

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeHead) {
    zend_long status;
    HashTable *headers = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_LONG(status)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY_HT(headers)
    ZEND_PARSE_PARAMETERS_END();

    const rapira_held *held = rapira_held_of(Z_OBJ_P(ZEND_THIS));
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_exchange_write_head(held, (int64_t)status, headers),
        "writeHead");
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeBody) {
    zend_string *content;
    bool eos = true;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_STR(content)
    Z_PARAM_OPTIONAL
    Z_PARAM_BOOL(eos)
    ZEND_PARSE_PARAMETERS_END();

    const rapira_held *held = rapira_held_of(Z_OBJ_P(ZEND_THIS));
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_exchange_write_body(held, content, eos), "writeBody");
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, sendFile) {
    zend_string *path;
    zend_long offset = 0;
    zend_long length = 0;
    bool length_is_null = true;
    bool eos = true;
    ZEND_PARSE_PARAMETERS_START(1, 4)
    Z_PARAM_PATH_STR(path)
    Z_PARAM_OPTIONAL
    Z_PARAM_LONG(offset)
    Z_PARAM_LONG_OR_NULL(length, length_is_null)
    Z_PARAM_BOOL(eos)
    ZEND_PARSE_PARAMETERS_END();

    if (offset < 0) {
        zend_argument_value_error(2, "must be greater than or equal to 0");
        RETURN_THROWS();
    }
    if (!length_is_null && length < 1) {
        zend_argument_value_error(3, "must be greater than or equal to 1");
        RETURN_THROWS();
    }

    // On ZTS a relative path starts in the PHP thread's working directory. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_virtual_cwd.c#L248-L285
    size_t path_len = ZSTR_LEN(path);
    char *cwd = NULL;
#ifdef VIRTUAL_DIR
    if (path_len > 0 && !IS_ABSOLUTE_PATH(ZSTR_VAL(path), path_len)) {
        size_t cwd_len;
        cwd = virtual_getcwd_ex(&cwd_len);
        if (!cwd) {
            // A thread without a cwd resolves no relative path.
            path_len = 0;
        }
    }
#endif

    const rapira_held *held = rapira_held_of(Z_OBJ_P(ZEND_THIS));
    bool sent = rapira_rs_exchange_send_file(
        held, ZSTR_VAL(path), path_len, cwd, (int64_t)offset, (int64_t)length,
        length_is_null, eos);
    if (cwd) {
        efree(cwd);
    }
    RAPIRA_RETURN_THROWS_UNLESS(sent, "sendFile");
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeTrailers) {
    HashTable *trailers;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_ARRAY_HT(trailers)
    ZEND_PARSE_PARAMETERS_END();

    const rapira_held *held = rapira_held_of(Z_OBJ_P(ZEND_THIS));
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_exchange_write_trailers(held, trailers), "writeTrailers");
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, flush) {
    ZEND_PARSE_PARAMETERS_NONE();
    const rapira_held *held = rapira_held_of(Z_OBJ_P(ZEND_THIS));
    RAPIRA_RETURN_THROWS_UNLESS(rapira_rs_exchange_flush(held), "flush");
}

// Rust builds the request graph (src/php/). This shell owns memo slot 0 (Request).
ZEND_METHOD(Rapira_Internal_Http_Exchange, getRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_unit_obj *ex = rapira_unit_from(Z_OBJ_P(ZEND_THIS));
    if (Z_ISUNDEF(ex->memos[0])) {
        RAPIRA_RETURN_THROWS_UNLESS(
            rapira_rs_exchange_build_request(&ex->held, &ex->memos[0]),
            "getRequest");
    }
    RETURN_COPY(&ex->memos[0]);
}
