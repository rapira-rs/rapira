#include "ext/spl/spl_array.h"
#include "rapira_grpc.h"
#include "zend_API.h"
#include "zend_enum.h"
#include "zend_types.h"

// rust glue (src/values.rs): false means a PHP exception is pending
extern bool rapira_rs_ctor_grpc_error_detail(zend_object *obj,
                                             zend_string *type_url,
                                             zend_string *value);
extern bool rapira_rs_ctor_grpc_status(zend_object *obj, zval *code,
                                       zend_string *message, zval *details);
extern bool rapira_rs_ctor_grpc_metadata(zend_object *obj, zval *entries);
extern bool rapira_rs_grpc_metadata_values(HashTable *entries, const char *name,
                                           size_t len, zval *rv);
extern bool rapira_rs_ctor_grpc_method_info(zend_object *obj, zend_string *name,
                                            zend_string *input_type,
                                            zend_string *output_type,
                                            zval *kind);
extern bool rapira_rs_ctor_grpc_service_info(zend_object *obj,
                                             zend_string *name, zval *methods);
extern bool rapira_rs_ctor_grpc_context(zend_object *obj, zend_string *method,
                                        zval *metadata, const double *deadline,
                                        zval *remote, zval *tls, zval *protocol,
                                        double received_at);
extern bool rapira_rs_ctor_grpc_exception(zend_object *obj, zval *code,
                                          zend_string *message, zval *details);
extern bool rapira_rs_grpc_kind_streams(const char *value, size_t len,
                                        bool request);

// rust glue (src/exchange/grpc.rs): false means a PHP exception is pending
extern bool rapira_rs_grpc_services(zval *rv);
extern bool rapira_rs_grpc_is_finalized(const void *state);
extern bool rapira_rs_grpc_is_cancelled(const void *state);
extern const char *rapira_rs_grpc_message(const void *state, size_t *len);
extern bool rapira_rs_grpc_context(rapira_grpc_call_obj *call, zval *rv);
extern bool rapira_rs_grpc_response_metadata(rapira_grpc_call_obj *call,
                                             zval *rv);
extern bool rapira_rs_grpc_respond(void *state, const char *message,
                                   size_t len);
extern bool rapira_rs_grpc_fail(void *state, zend_object *status);
extern bool rapira_rs_grpc_add(void *state, bool trailer, bool binary,
                               const char *name, size_t name_len,
                               const char *value, size_t value_len);
extern bool rapira_rs_grpc_snapshot(const void *state, bool trailer, zval *rv);

// $entries is property slot 0 and the constructor always sets it. An instance
// from ReflectionClass::newInstanceWithoutConstructor() is not supported.
static zend_always_inline zval *
rapira_grpc_metadata_entries(zend_object *metadata) {
    return OBJ_PROP_NUM(metadata, 0);
}

ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    zval *value = zend_enum_fetch_case_value(Z_OBJ_P(ZEND_THIS));
    RETURN_BOOL(rapira_rs_grpc_kind_streams(Z_STRVAL_P(value),
                                            Z_STRLEN_P(value), true));
}

ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingResponse) {
    ZEND_PARSE_PARAMETERS_NONE();
    zval *value = zend_enum_fetch_case_value(Z_OBJ_P(ZEND_THIS));
    RETURN_BOOL(rapira_rs_grpc_kind_streams(Z_STRVAL_P(value),
                                            Z_STRLEN_P(value), false));
}

ZEND_METHOD(Rapira_Grpc_ErrorDetail, __construct) {
    zend_string *type_url, *value;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(type_url)
    Z_PARAM_STR(value)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_grpc_error_detail(Z_OBJ_P(ZEND_THIS), type_url,
                                          value)) {
        rapira_throw_or_backstop("ErrorDetail construction");
        RETURN_THROWS();
    }
}

// Status and GrpcException take the same (StatusCode, message, details) triple.
static void grpc_status_ctor(INTERNAL_FUNCTION_PARAMETERS,
                             bool (*ctor)(zend_object *, zval *, zend_string *,
                                          zval *),
                             const char *what) {
    zval *code;
    zend_string *message = ZSTR_EMPTY_ALLOC();
    zval *details = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 3)
    Z_PARAM_OBJECT_OF_CLASS(code, rapira_ce_grpc_status_code)
    Z_PARAM_OPTIONAL
    Z_PARAM_STR(message)
    Z_PARAM_ARRAY(details)
    ZEND_PARSE_PARAMETERS_END();

    zval empty;
    if (details == NULL) {
        ZVAL_EMPTY_ARRAY(&empty);
        details = &empty;
    }
    if (!ctor(Z_OBJ_P(ZEND_THIS), code, message, details)) {
        rapira_throw_or_backstop(what);
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Status, __construct) {
    grpc_status_ctor(INTERNAL_FUNCTION_PARAM_PASSTHRU, rapira_rs_ctor_grpc_status,
                     "Status construction");
}

ZEND_METHOD(Rapira_Grpc_Metadata, __construct) {
    zval *entries = NULL;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY(entries)
    ZEND_PARSE_PARAMETERS_END();

    zval empty;
    if (entries == NULL) {
        ZVAL_EMPTY_ARRAY(&empty);
        entries = &empty;
    }
    if (!rapira_rs_ctor_grpc_metadata(Z_OBJ_P(ZEND_THIS), entries)) {
        rapira_throw_or_backstop("Metadata construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Metadata, values) {
    zend_string *name;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(name)
    ZEND_PARSE_PARAMETERS_END();

    zval *entries = rapira_grpc_metadata_entries(Z_OBJ_P(ZEND_THIS));
    if (!rapira_rs_grpc_metadata_values(Z_ARRVAL_P(entries), ZSTR_VAL(name),
                                        ZSTR_LEN(name), return_value)) {
        rapira_throw_or_backstop("Metadata::values");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Metadata, count) {
    ZEND_PARSE_PARAMETERS_NONE();
    zval *entries = rapira_grpc_metadata_entries(Z_OBJ_P(ZEND_THIS));
    RETURN_LONG(zend_hash_num_elements(Z_ARRVAL_P(entries)));
}

ZEND_METHOD(Rapira_Grpc_Metadata, getIterator) {
    ZEND_PARSE_PARAMETERS_NONE();
    zval *entries = rapira_grpc_metadata_entries(Z_OBJ_P(ZEND_THIS));
    object_init_ex(return_value, spl_ce_ArrayIterator);
    zend_call_known_instance_method_with_1_params(
        spl_ce_ArrayIterator->constructor, Z_OBJ_P(return_value), NULL,
        entries);
}

ZEND_METHOD(Rapira_Grpc_MethodInfo, __construct) {
    zend_string *name, *input_type, *output_type;
    zval *kind;
    ZEND_PARSE_PARAMETERS_START(4, 4)
    Z_PARAM_STR(name)
    Z_PARAM_STR(input_type)
    Z_PARAM_STR(output_type)
    Z_PARAM_OBJECT_OF_CLASS(kind, rapira_ce_grpc_method_kind)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_grpc_method_info(Z_OBJ_P(ZEND_THIS), name, input_type,
                                         output_type, kind)) {
        rapira_throw_or_backstop("MethodInfo construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_ServiceInfo, __construct) {
    zend_string *name;
    zval *methods;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(name)
    Z_PARAM_ARRAY(methods)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_grpc_service_info(Z_OBJ_P(ZEND_THIS), name, methods)) {
        rapira_throw_or_backstop("ServiceInfo construction");
        RETURN_THROWS();
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

    if (!rapira_rs_ctor_grpc_context(Z_OBJ_P(ZEND_THIS), method, metadata,
                                     !deadline_null ? &deadline : NULL, remote,
                                     tls, protocol, received_at)) {
        rapira_throw_or_backstop("Context construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Exception_GrpcException, __construct) {
    grpc_status_ctor(INTERNAL_FUNCTION_PARAM_PASSTHRU,
                     rapira_rs_ctor_grpc_exception,
                     "GrpcException construction");
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, name) {
    ZEND_PARSE_PARAMETERS_NONE();
    // the plugin's root TOML section
    RETURN_STRING("grpc");
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, getServices) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_services(return_value)) {
        rapira_throw_or_backstop("getServices");
        RETURN_THROWS();
    }
}

// receive() sets state before PHP code can reach the object
static void *grpc_call_state(zval *this_ptr) {
    void *state = rapira_grpc_call_from(Z_OBJ_P(this_ptr))->state;
    ZEND_ASSERT(state != NULL);
    return state;
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, isFinalized) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_grpc_is_finalized(grpc_call_state(ZEND_THIS)));
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, isCancelled) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_grpc_is_cancelled(grpc_call_state(ZEND_THIS)));
}

// One zend_string for the life of the call object: every call shares it.
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getMessage) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_grpc_call_obj *call = rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS));
    if (Z_ISUNDEF(call->message)) {
        size_t len = 0;
        const char *message =
            rapira_rs_grpc_message(grpc_call_state(ZEND_THIS), &len);
        ZVAL_STRINGL(&call->message, message, len);
    }
    RETURN_COPY(&call->message);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getContext) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_context(rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS)),
                                return_value)) {
        rapira_throw_or_backstop("getContext");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getResponseMetadata) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (!rapira_rs_grpc_response_metadata(
            rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS)), return_value)) {
        rapira_throw_or_backstop("getResponseMetadata");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, respond) {
    zend_string *message;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(message)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_respond(grpc_call_state(ZEND_THIS), ZSTR_VAL(message),
                                ZSTR_LEN(message))) {
        rapira_throw_or_backstop("respond");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, fail) {
    zval *status;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_OBJECT_OF_CLASS(status, rapira_ce_grpc_status)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_grpc_fail(grpc_call_state(ZEND_THIS), Z_OBJ_P(status))) {
        rapira_throw_or_backstop("fail");
        RETURN_THROWS();
    }
}

// state is NULL after the call object is gone; Rust then throws
// AlreadyFinalizedError
static void grpc_add(INTERNAL_FUNCTION_PARAMETERS, bool trailer, bool binary,
                     const char *what) {
    zend_string *name, *value;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(name)
    Z_PARAM_STR(value)
    ZEND_PARSE_PARAMETERS_END();

    void *state = rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->state;
    if (!rapira_rs_grpc_add(state, trailer, binary, ZSTR_VAL(name),
                            ZSTR_LEN(name), ZSTR_VAL(value), ZSTR_LEN(value))) {
        rapira_throw_or_backstop(what);
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addHeader) {
    grpc_add(INTERNAL_FUNCTION_PARAM_PASSTHRU, false, false, "addHeader");
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addBinaryHeader) {
    grpc_add(INTERNAL_FUNCTION_PARAM_PASSTHRU, false, true, "addBinaryHeader");
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addTrailer) {
    grpc_add(INTERNAL_FUNCTION_PARAM_PASSTHRU, true, false, "addTrailer");
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addBinaryTrailer) {
    grpc_add(INTERNAL_FUNCTION_PARAM_PASSTHRU, true, true, "addBinaryTrailer");
}

// state is NULL after the call object is gone; the snapshot is then empty
static void grpc_snapshot(INTERNAL_FUNCTION_PARAMETERS, bool trailer,
                          const char *what) {
    ZEND_PARSE_PARAMETERS_NONE();
    void *state = rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->state;
    if (!rapira_rs_grpc_snapshot(state, trailer, return_value)) {
        rapira_throw_or_backstop(what);
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, headers) {
    grpc_snapshot(INTERNAL_FUNCTION_PARAM_PASSTHRU, false, "headers");
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, trailers) {
    grpc_snapshot(INTERNAL_FUNCTION_PARAM_PASSTHRU, true, "trailers");
}
