#include "ext/spl/spl_array.h"
#include "rapira_classes.h"
#include "zend_enum.h"

extern bool rapira_rs_grpc_metadata_construct(zend_object *obj,
                                              HashTable *entries);
extern void rapira_rs_grpc_metadata_values(zend_object *obj, zend_string *name,
                                           zval *out);
extern zend_long rapira_rs_grpc_metadata_count(zend_object *obj);
extern void rapira_rs_grpc_metadata_iterator(zend_object *obj, zval *out);

void rapira_array_iterator(zval *out, zval *entries) {
    object_init_ex(out, spl_ce_ArrayIterator);
    zend_call_method_with_1_params(Z_OBJ_P(out), spl_ce_ArrayIterator,
                                   &spl_ce_ArrayIterator->constructor,
                                   "__construct", NULL, entries);
}

ZEND_METHOD(Rapira_Grpc_Metadata, __construct) {
    HashTable *entries = NULL;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY_HT(entries)
    ZEND_PARSE_PARAMETERS_END();
    if (!rapira_rs_grpc_metadata_construct(Z_OBJ_P(ZEND_THIS), entries)) {
        rapira_throw_or_backstop("Metadata construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Metadata, values) {
    zend_string *name;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(name)
    ZEND_PARSE_PARAMETERS_END();
    rapira_rs_grpc_metadata_values(Z_OBJ_P(ZEND_THIS), name, return_value);
}

ZEND_METHOD(Rapira_Grpc_Metadata, count) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_rs_grpc_metadata_count(Z_OBJ_P(ZEND_THIS)));
}

ZEND_METHOD(Rapira_Grpc_Metadata, getIterator) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_rs_grpc_metadata_iterator(Z_OBJ_P(ZEND_THIS), return_value);
}

extern bool rapira_rs_grpc_ctor_error_detail(zend_object *obj,
                                             zend_string *type,
                                             zend_string *value);
extern bool rapira_rs_grpc_ctor_status(zend_object *obj, zval *code,
                                       zend_string *message,
                                       HashTable *details);
extern bool rapira_rs_grpc_ctor_exception(zend_object *obj, zval *code,
                                          zend_string *message,
                                          HashTable *details);
extern bool rapira_rs_grpc_ctor_method(zend_object *obj, zend_string *name,
                                       zend_string *input, zend_string *output,
                                       zval *kind);
extern bool rapira_rs_grpc_ctor_service(zend_object *obj, zend_string *name,
                                        HashTable *methods);
extern bool rapira_rs_grpc_method_streaming(zend_string *kind, bool request);

ZEND_METHOD(Rapira_Grpc_ErrorDetail, __construct) {
    zend_string *type, *value;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(type)
    Z_PARAM_STR(value)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_ctor_error_detail(Z_OBJ_P(ZEND_THIS), type, value)) {
        rapira_throw_or_backstop("ErrorDetail construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Status, __construct) {
    zval *code;
    zend_string *message = NULL;
    HashTable *details = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 3)
    Z_PARAM_OBJECT_OF_CLASS(code, rapira_ce_grpc_status_code)
    Z_PARAM_OPTIONAL
    Z_PARAM_STR(message)
    Z_PARAM_ARRAY_HT(details)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_ctor_status(Z_OBJ_P(ZEND_THIS), code, message,
                                    details)) {
        rapira_throw_or_backstop("Status construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Exception_GrpcException, __construct) {
    zval *code;
    zend_string *message = NULL;
    HashTable *details = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 3)
    Z_PARAM_OBJECT_OF_CLASS(code, rapira_ce_grpc_status_code)
    Z_PARAM_OPTIONAL
    Z_PARAM_STR(message)
    Z_PARAM_ARRAY_HT(details)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_ctor_exception(Z_OBJ_P(ZEND_THIS), code, message,
                                       details)) {
        rapira_throw_or_backstop("GrpcException construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_MethodInfo, __construct) {
    zend_string *name, *input, *output;
    zval *kind;
    ZEND_PARSE_PARAMETERS_START(4, 4)
    Z_PARAM_STR(name)
    Z_PARAM_STR(input)
    Z_PARAM_STR(output)
    Z_PARAM_OBJECT_OF_CLASS(kind, rapira_ce_grpc_method_kind)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_ctor_method(Z_OBJ_P(ZEND_THIS), name, input, output,
                                    kind)) {
        rapira_throw_or_backstop("MethodInfo construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_ServiceInfo, __construct) {
    zend_string *name;
    HashTable *methods;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(name)
    Z_PARAM_ARRAY_HT(methods)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_ctor_service(Z_OBJ_P(ZEND_THIS), name, methods)) {
        rapira_throw_or_backstop("ServiceInfo construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_grpc_method_streaming(
        Z_STR_P(zend_enum_fetch_case_value(Z_OBJ_P(ZEND_THIS))), true));
}

ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingResponse) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_grpc_method_streaming(
        Z_STR_P(zend_enum_fetch_case_value(Z_OBJ_P(ZEND_THIS))), false));
}

extern bool rapira_rs_grpc_receive(int64_t timeout, bool poll, zval *out);
extern void rapira_rs_grpc_info(zval *out);
extern bool rapira_rs_grpc_services(zval *out);
extern bool rapira_rs_grpc_context(rapira_grpc_call_obj *call, zval *out);
extern void rapira_rs_grpc_response_metadata(rapira_grpc_call_obj *call,
                                             zval *out);
extern const char *rapira_rs_grpc_message(void *state, size_t *len);
extern zend_string *rapira_rs_grpc_message_buffer(const char *bytes, size_t len);
extern bool rapira_rs_grpc_cancelled(void *state);
extern bool rapira_rs_grpc_finalized(void *state);
extern bool rapira_rs_grpc_respond(void *state, zend_string *message);
extern bool rapira_rs_grpc_fail(void *state, zend_object *status);
extern bool rapira_rs_grpc_add_metadata(void *state, zend_string *name,
                                        zend_string *value, bool binary,
                                        bool trailer);
extern bool rapira_rs_grpc_metadata_snapshot(void *state, bool trailer,
                                             zval *out);
extern bool rapira_rs_grpc_ctor_context(zend_object *obj, zend_string *method,
                                        zval *metadata, double deadline,
                                        bool deadline_null, zval *remote,
                                        zval *tls, zval *protocol,
                                        double received_at);

// The caller owns Rust input until this bracket returns, including on bailout.
bool rapira_grpc_build(void (*build)(const void *, zval *), const void *data,
                       zval *out) {
    zend_try { build(data, out); }
    zend_catch { return false; }
    zend_end_try();
    return true;
}

zend_string *rapira_grpc_message_alloc(size_t len) {
    zend_string *string;
    zend_try { string = zend_string_alloc(len, false); }
    zend_catch { return NULL; }
    zend_end_try();
    return string;
}

zend_string *rapira_grpc_message_share(zend_string *string, size_t len) {
    ZSTR_LEN(string) = len;
    ZSTR_VAL(string)[len] = '\0';
    // This clears the hash and UTF-8 flag, as in zend_string_separate().
    // https://github.com/php/php-src/blob/php-8.5.10/Zend/zend_string.h#L135-L139
    zend_string_forget_hash_val(string);
    return zend_string_copy(string);
}

void rapira_grpc_message_release(zend_string *string) {
    zend_string_release(string);
}

void rapira_enum_case(zend_class_entry *ce, const char *name, zval *out) {
    ZVAL_OBJ_COPY(out, zend_enum_get_case_cstr(ce, name));
}

#define GRPC_HOST_CTOR(type)                                                   \
    ZEND_METHOD(type, __construct) {                                           \
        (void)execute_data;                                                    \
        (void)return_value;                                                    \
        zend_throw_error(NULL, "host-created");                                \
    }

GRPC_HOST_CTOR(Rapira_Internal_Grpc_Dispatcher)
GRPC_HOST_CTOR(Rapira_Internal_Grpc_DispatcherInfo)
GRPC_HOST_CTOR(Rapira_Internal_Grpc_UnaryCall)
GRPC_HOST_CTOR(Rapira_Internal_Grpc_ResponseMetadata)

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, name) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_STRING("grpc");
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, receive) {
    zend_long timeout = -1;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_LONG(timeout)
    ZEND_PARSE_PARAMETERS_END();
    if (!rapira_rs_grpc_receive(timeout, false, return_value)) {
        rapira_throw_or_backstop("receive");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, tryReceive) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_receive(0, true, return_value)) {
        rapira_throw_or_backstop("tryReceive");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, getInfo) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_rs_grpc_info(return_value);
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, getServices) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_services(return_value)) {
        zend_bailout();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_DispatcherInfo, pendingCount) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->pending);
}

ZEND_METHOD(Rapira_Internal_Grpc_DispatcherInfo, activeCount) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->active);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getContext) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_context(rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS)),
                                return_value)) {
        zend_bailout();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getResponseMetadata) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_rs_grpc_response_metadata(rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS)),
                                     return_value);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getMessage) {
    ZEND_PARSE_PARAMETERS_NONE();
    size_t len;
    const char *message = rapira_rs_grpc_message(
        rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS))->state, &len);
    if (ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len)) <= ZEND_MM_MAX_LARGE_SIZE) {
        RETURN_STRINGL_FAST(message, len);
    }
    zend_string *string = rapira_rs_grpc_message_buffer(message, len);
    if (!string) {
        zend_bailout();
    }
    RETURN_STR(string);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, isCancelled) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_grpc_cancelled(
        rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS))->state));
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, isFinalized) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_grpc_finalized(
        rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS))->state));
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, __destruct) {
    (void)return_value;
    ZEND_PARSE_PARAMETERS_NONE();
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, respond) {
    zend_string *message;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(message)
    ZEND_PARSE_PARAMETERS_END();
    if (!rapira_rs_grpc_respond(
            rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS))->state, message)) {
        rapira_throw_or_backstop("respond");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, fail) {
    zval *status;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_OBJECT_OF_CLASS(status, rapira_ce_grpc_status)
    ZEND_PARSE_PARAMETERS_END();
    if (!rapira_rs_grpc_fail(rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS))->state,
                             Z_OBJ_P(status))) {
        rapira_throw_or_backstop("fail");
        RETURN_THROWS();
    }
}

#define GRPC_METADATA_ADD(method, binary, trailer)                             \
    ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, method) {               \
        zend_string *name, *value;                                             \
        ZEND_PARSE_PARAMETERS_START(2, 2)                                      \
        Z_PARAM_STR(name)                                                      \
        Z_PARAM_STR(value)                                                     \
        ZEND_PARSE_PARAMETERS_END();                                           \
        if (!rapira_rs_grpc_add_metadata(                                      \
                rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->state, name,    \
                value, binary, trailer)) {                                     \
            rapira_throw_or_backstop(#method);                                 \
            RETURN_THROWS();                                                   \
        }                                                                      \
    }

GRPC_METADATA_ADD(addHeader, false, false)
GRPC_METADATA_ADD(addBinaryHeader, true, false)
GRPC_METADATA_ADD(addTrailer, false, true)
GRPC_METADATA_ADD(addBinaryTrailer, true, true)

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, headers) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_metadata_snapshot(
            rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->state, false,
            return_value)) {
        zend_bailout();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, trailers) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_metadata_snapshot(
            rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->state, true,
            return_value)) {
        zend_bailout();
    }
}

ZEND_METHOD(Rapira_Grpc_Call_Context, __construct) {
    zend_string *method;
    zval *metadata, *remote, *tls, *protocol;
    double deadline, received_at;
    bool deadline_null;
    ZEND_PARSE_PARAMETERS_START(7, 7)
    Z_PARAM_STR(method)
    Z_PARAM_OBJECT_OF_CLASS(metadata, rapira_ce_grpc_metadata)
    Z_PARAM_DOUBLE_OR_NULL(deadline, deadline_null)
    Z_PARAM_ZVAL(remote)
    Z_PARAM_OBJECT_OF_CLASS_OR_NULL(tls, rapira_ce_tls)
    Z_PARAM_OBJECT_OF_CLASS(protocol, rapira_ce_grpc_protocol)
    Z_PARAM_DOUBLE(received_at)
    ZEND_PARSE_PARAMETERS_END();
    if (!rapira_rs_grpc_ctor_context(Z_OBJ_P(ZEND_THIS), method, metadata,
                                     deadline, deadline_null, remote, tls,
                                     protocol, received_at)) {
        rapira_throw_or_backstop("Context construction");
        RETURN_THROWS();
    }
}
