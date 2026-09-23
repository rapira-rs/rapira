#include "rapira_classes.h"
#include "wrapper.h"

#include "zend_API.h"

// rust glue; the verbs throw from Rust and report false with the throw pending
extern bool rapira_rs_exchange_build_request(rapira_exchange_obj *ex,
                                             zval *return_value);
extern bool rapira_rs_exchange_write_head(void *job, int64_t status,
                                          HashTable *headers);
extern bool rapira_rs_exchange_write_body(void *job, const char *p, size_t len,
                                          bool eos);
extern bool rapira_rs_exchange_is_finalized(const void *job);
extern bool rapira_rs_exchange_is_cancelled(const void *job);
extern bool rapira_rs_exchange_flush(void *job);
extern bool rapira_rs_exchange_send_file(void *job, const char *path,
                                         size_t path_len, int64_t offset,
                                         int64_t length, bool length_is_null,
                                         bool eos);
extern bool rapira_rs_exchange_write_trailers(void *job, HashTable *trailers);

ZEND_METHOD(Rapira_Internal_Http_Exchange, __construct) {
    (void)execute_data;
    (void)return_value;
    zend_throw_error(NULL, "host-created");
}

// receive() sets job before PHP code can reach the object
static void *exchange_job(zval *this_ptr) {
    void *job = rapira_exchange_from(Z_OBJ_P(this_ptr))->job;
    ZEND_ASSERT(job != NULL);
    return job;
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeHead) {
    zend_long status;
    HashTable *headers = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_LONG(status)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY_HT(headers)
    ZEND_PARSE_PARAMETERS_END();

    void *job = exchange_job(ZEND_THIS);
    if (!rapira_rs_exchange_write_head(job, (int64_t)status, headers)) {
        rapira_throw_or_backstop("writeHead");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeBody) {
    zend_string *content;
    bool eos = true;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_STR(content)
    Z_PARAM_OPTIONAL
    Z_PARAM_BOOL(eos)
    ZEND_PARSE_PARAMETERS_END();

    void *job = exchange_job(ZEND_THIS);
    if (!rapira_rs_exchange_write_body(job, ZSTR_VAL(content),
                                       ZSTR_LEN(content), eos)) {
        rapira_throw_or_backstop("writeBody");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, isFinalized) {
    ZEND_PARSE_PARAMETERS_NONE();
    void *job = exchange_job(ZEND_THIS);
    RETURN_BOOL(rapira_rs_exchange_is_finalized(job));
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, isCancelled) {
    ZEND_PARSE_PARAMETERS_NONE();
    void *job = exchange_job(ZEND_THIS);
    RETURN_BOOL(rapira_rs_exchange_is_cancelled(job));
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

    void *job = exchange_job(ZEND_THIS);
    if (!rapira_rs_exchange_send_file(job, ZSTR_VAL(path), ZSTR_LEN(path),
                                      (int64_t)offset, (int64_t)length,
                                      length_is_null, eos)) {
        rapira_throw_or_backstop("sendFile");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeTrailers) {
    HashTable *trailers;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_ARRAY_HT(trailers)
    ZEND_PARSE_PARAMETERS_END();

    void *job = exchange_job(ZEND_THIS);
    if (!rapira_rs_exchange_write_trailers(job, trailers)) {
        rapira_throw_or_backstop("writeTrailers");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, flush) {
    ZEND_PARSE_PARAMETERS_NONE();
    void *job = exchange_job(ZEND_THIS);
    if (!rapira_rs_exchange_flush(job)) {
        rapira_throw_or_backstop("flush");
        RETURN_THROWS();
    }
}

// the request graph builder lives in Rust (src/exchange/); this shell owns the macro layer
ZEND_METHOD(Rapira_Internal_Http_Exchange, getRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_exchange_obj *ex = rapira_exchange_from(Z_OBJ_P(ZEND_THIS));
    if (!rapira_rs_exchange_build_request(ex, return_value)) {
        rapira_throw_or_backstop("getRequest");
        RETURN_THROWS();
    }
}
