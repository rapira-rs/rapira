#include "rapira_http.h"

// rust glue; the verbs throw from Rust and report false with the throw pending
extern bool rapira_rs_exchange_build_request(const rapira_held *held,
                                             zval *memo);
extern bool rapira_rs_exchange_write_head(const rapira_held *held,
                                          int64_t status, HashTable *headers);
extern bool rapira_rs_exchange_write_body(const rapira_held *held,
                                          const char *p, size_t len, bool eos);
extern bool rapira_rs_exchange_flush(const rapira_held *held);
extern bool rapira_rs_exchange_send_file(const rapira_held *held,
                                         const char *path, size_t path_len,
                                         int64_t offset, int64_t length,
                                         bool length_is_null, bool eos);
extern bool rapira_rs_exchange_write_trailers(const rapira_held *held,
                                              HashTable *trailers);

// receive() fills the slot before PHP code can reach the object
static const rapira_held *exchange_held(zval *this_ptr) {
    return &rapira_exchange_from(Z_OBJ_P(this_ptr))->held;
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeHead) {
    zend_long status;
    HashTable *headers = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_LONG(status)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY_HT(headers)
    ZEND_PARSE_PARAMETERS_END();

    const rapira_held *held = exchange_held(ZEND_THIS);
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

    const rapira_held *held = exchange_held(ZEND_THIS);
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_exchange_write_body(held, ZSTR_VAL(content), ZSTR_LEN(content),
                                      eos),
        "writeBody");
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

    const rapira_held *held = exchange_held(ZEND_THIS);
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_exchange_send_file(held, ZSTR_VAL(path), ZSTR_LEN(path),
                                     (int64_t)offset, (int64_t)length,
                                     length_is_null, eos),
        "sendFile");
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeTrailers) {
    HashTable *trailers;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_ARRAY_HT(trailers)
    ZEND_PARSE_PARAMETERS_END();

    const rapira_held *held = exchange_held(ZEND_THIS);
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_exchange_write_trailers(held, trailers), "writeTrailers");
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, flush) {
    ZEND_PARSE_PARAMETERS_NONE();
    const rapira_held *held = exchange_held(ZEND_THIS);
    RAPIRA_RETURN_THROWS_UNLESS(rapira_rs_exchange_flush(held), "flush");
}

// the request graph builder lives in Rust (src/php/); this shell owns the memo
ZEND_METHOD(Rapira_Internal_Http_Exchange, getRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_exchange_obj *ex = rapira_exchange_from(Z_OBJ_P(ZEND_THIS));
    if (Z_ISUNDEF(ex->request)) {
        RAPIRA_RETURN_THROWS_UNLESS(
            rapira_rs_exchange_build_request(&ex->held, &ex->request),
            "getRequest");
    }
    RETURN_COPY(&ex->request);
}
