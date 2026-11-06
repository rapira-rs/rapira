#include "rapira_grpc.h"
#include "ext/spl/spl_array.h"

// rust glue (src/php/values.rs): returns false with a PHP exception pending
extern bool rapira_rs_ctor_grpc_metadata(zend_object *obj, zval *entries);

// rust glue (src/php/call.rs): message returns a plain value; the others
// return false with a PHP exception pending
extern bool rapira_rs_grpc_services(zval *rv);
extern const char *rapira_rs_grpc_message(const rapira_held *held, size_t *len);
extern bool rapira_rs_grpc_context(const rapira_held *held, zval *memo);
extern bool rapira_rs_grpc_respond(const rapira_held *held, const char *message,
                                   size_t len);
extern bool rapira_rs_grpc_fail(const rapira_held *held, zend_object *status);
extern bool rapira_rs_grpc_add(const rapira_held *held, bool trailer,
                               bool binary, const char *name, size_t name_len,
                               const char *value, size_t value_len);
extern bool rapira_rs_grpc_snapshot(const rapira_held *held, bool trailer,
                                    zval *rv);

// $entries is property slot 0 and the constructor always sets it.
static zend_always_inline zval *
rapira_grpc_metadata_entries(zend_object *metadata) {
    return OBJ_PROP_NUM(metadata, 0);
}

// client and bidi streaming stream the request; server and bidi streaming
// stream the response
static bool grpc_kind_streams(zval *this_ptr, bool request) {
    zend_string *kind = Z_STR_P(zend_enum_fetch_case_value(Z_OBJ_P(this_ptr)));
    if (zend_string_equals_literal(kind, "bidi-streaming")) {
        return true;
    }
    if (request) {
        return zend_string_equals_literal(kind, "client-streaming");
    }
    return zend_string_equals_literal(kind, "server-streaming");
}

ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(grpc_kind_streams(ZEND_THIS, true));
}

ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingResponse) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(grpc_kind_streams(ZEND_THIS, false));
}

ZEND_METHOD(Rapira_Grpc_ErrorDetail, __construct) {
    zend_string *type_url, *value;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(type_url)
    Z_PARAM_STR(value)
    ZEND_PARSE_PARAMETERS_END();

    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(obj->ce, obj, ZEND_STRL("typeUrl"), type_url);
    zend_update_property_str(obj->ce, obj, ZEND_STRL("value"), value);
}

// writes the (StatusCode, message, details) triple to a Status object
static void grpc_status_props(zend_object *obj, zval *code,
                              zend_string *message, zval *details) {
    zend_class_entry *ce = obj->ce;
    zend_update_property(ce, obj, ZEND_STRL("code"), code);
    zend_update_property_str(ce, obj, ZEND_STRL("message"), message);
    zend_update_property(ce, obj, ZEND_STRL("details"), details);
}

// Writes message in the exception base scope, as Exception::__construct does.
// Writes the readonly status in the GrpcException scope, so a userland
// subclass can call parent::__construct().
static void grpc_exception_props(zend_object *obj, zval *code,
                                 zend_string *message, zval *details) {
    zend_update_property_str(zend_get_exception_base(obj), obj,
                             ZEND_STRL("message"), message);
    zval status;
    object_init_ex(&status, rapira_ce_grpc_status);
    grpc_status_props(Z_OBJ(status), code, message, details);
    zend_update_property(rapira_ce_grpc_exception, obj, ZEND_STRL("status"),
                         &status);
    zval_ptr_dtor(&status);
}

// Status and GrpcException take the same (StatusCode, message, details) triple.
static void grpc_status_ctor(INTERNAL_FUNCTION_PARAMETERS,
                             void (*props)(zend_object *, zval *, zend_string *,
                                           zval *)) {
    zval *code;
    zend_string *message = ZSTR_EMPTY_ALLOC();
    zval empty;
    ZVAL_EMPTY_ARRAY(&empty);
    zval *details = &empty;
    ZEND_PARSE_PARAMETERS_START(1, 3)
    Z_PARAM_OBJECT_OF_CLASS(code, rapira_ce_grpc_status_code)
    Z_PARAM_OPTIONAL
    Z_PARAM_STR(message)
    Z_PARAM_ARRAY(details)
    ZEND_PARSE_PARAMETERS_END();

    props(Z_OBJ_P(ZEND_THIS), code, message, details);
}

ZEND_METHOD(Rapira_Grpc_Status, __construct) {
    grpc_status_ctor(INTERNAL_FUNCTION_PARAM_PASSTHRU, grpc_status_props);
}

ZEND_METHOD(Rapira_Grpc_Metadata, __construct) {
    zval empty;
    ZVAL_EMPTY_ARRAY(&empty);
    zval *entries = &empty;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY(entries)
    ZEND_PARSE_PARAMETERS_END();

    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_ctor_grpc_metadata(Z_OBJ_P(ZEND_THIS), entries),
        "Metadata construction");
}

ZEND_METHOD(Rapira_Grpc_Metadata, values) {
    zend_string *name;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(name)
    ZEND_PARSE_PARAMETERS_END();

    zval *entries = rapira_grpc_metadata_entries(Z_OBJ_P(ZEND_THIS));
    zend_string *key = zend_string_tolower(name);
    zval *found = zend_symtable_find(Z_ARRVAL_P(entries), key);
    zend_string_release(key);
    if (found) {
        RETURN_COPY_DEREF(found);
    }
    RETURN_EMPTY_ARRAY();
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

    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_class_entry *ce = obj->ce;
    zend_update_property_str(ce, obj, ZEND_STRL("name"), name);
    zend_update_property_str(ce, obj, ZEND_STRL("inputType"), input_type);
    zend_update_property_str(ce, obj, ZEND_STRL("outputType"), output_type);
    zend_update_property(ce, obj, ZEND_STRL("kind"), kind);
}

ZEND_METHOD(Rapira_Grpc_ServiceInfo, __construct) {
    zend_string *name;
    zval *methods;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(name)
    Z_PARAM_ARRAY(methods)
    ZEND_PARSE_PARAMETERS_END();

    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(obj->ce, obj, ZEND_STRL("name"), name);
    zend_update_property(obj->ce, obj, ZEND_STRL("methods"), methods);
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

    if (!rapira_address_arg(remote, 4)) {
        RETURN_THROWS();
    }
    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_class_entry *ce = obj->ce;
    zend_update_property_str(ce, obj, ZEND_STRL("method"), method);
    zend_update_property(ce, obj, ZEND_STRL("metadata"), metadata);
    if (deadline_null) {
        zend_update_property_null(ce, obj, ZEND_STRL("deadline"));
    } else {
        zend_update_property_double(ce, obj, ZEND_STRL("deadline"), deadline);
    }
    zend_update_property(ce, obj, ZEND_STRL("remote"), remote);
    if (tls) {
        zend_update_property(ce, obj, ZEND_STRL("tls"), tls);
    } else {
        zend_update_property_null(ce, obj, ZEND_STRL("tls"));
    }
    zend_update_property(ce, obj, ZEND_STRL("protocol"), protocol);
    zend_update_property_double(ce, obj, ZEND_STRL("receivedAt"), received_at);
}

ZEND_METHOD(Rapira_Grpc_Exception_GrpcException, __construct) {
    grpc_status_ctor(INTERNAL_FUNCTION_PARAM_PASSTHRU, grpc_exception_props);
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, name) {
    ZEND_PARSE_PARAMETERS_NONE();
    // the plugin's root TOML section
    RETURN_STRING("grpc");
}

ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, getServices) {
    ZEND_PARSE_PARAMETERS_NONE();
    RAPIRA_RETURN_THROWS_UNLESS(rapira_rs_grpc_services(return_value),
                                "getServices");
}

// receive() fills the slot before PHP code can reach the object
static const rapira_held *grpc_call_held(zval *this_ptr) {
    return &rapira_grpc_call_from(Z_OBJ_P(this_ptr))->held;
}

// getMessage() copies the message into one zend_string on its first call.
// Later calls return the same zend_string.
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getMessage) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_grpc_call_obj *call = rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS));
    if (Z_ISUNDEF(call->message)) {
        size_t len = 0;
        const char *message =
            rapira_rs_grpc_message(grpc_call_held(ZEND_THIS), &len);
        ZVAL_STRINGL(&call->message, message, len);
    }
    RETURN_COPY(&call->message);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getContext) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_grpc_call_obj *call = rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS));
    if (Z_ISUNDEF(call->context)) {
        RAPIRA_RETURN_THROWS_UNLESS(
            rapira_rs_grpc_context(&call->held, &call->context), "getContext");
    }
    RETURN_COPY(&call->context);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getResponseMetadata) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_grpc_call_obj *call = rapira_grpc_call_from(Z_OBJ_P(ZEND_THIS));
    if (Z_ISUNDEF(call->metadata)) {
        object_init_ex(&call->metadata,
                       rapira_ce_internal_grpc_response_metadata);
        rapira_grpc_metadata_from(Z_OBJ(call->metadata))->held = call->held;
    }
    RETURN_COPY(&call->metadata);
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, respond) {
    zend_string *message;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR(message)
    ZEND_PARSE_PARAMETERS_END();

    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_grpc_respond(grpc_call_held(ZEND_THIS), ZSTR_VAL(message),
                               ZSTR_LEN(message)),
        "respond");
}

ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, fail) {
    zval *status;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_OBJECT_OF_CLASS(status, rapira_ce_grpc_status)
    ZEND_PARSE_PARAMETERS_END();

    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_grpc_fail(grpc_call_held(ZEND_THIS), Z_OBJ_P(status)),
        "fail");
}

// the free_obj of the call zeroes held; Rust then throws AlreadyFinalizedError
static void grpc_add(INTERNAL_FUNCTION_PARAMETERS, bool trailer, bool binary,
                     const char *what) {
    zend_string *name, *value;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(name)
    Z_PARAM_STR(value)
    ZEND_PARSE_PARAMETERS_END();

    const rapira_held *held =
        &rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->held;
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_grpc_add(held, trailer, binary, ZSTR_VAL(name),
                           ZSTR_LEN(name), ZSTR_VAL(value), ZSTR_LEN(value)),
        what);
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

// the free_obj of the call zeroes held; the snapshot is then empty
static void grpc_snapshot(INTERNAL_FUNCTION_PARAMETERS, bool trailer,
                          const char *what) {
    ZEND_PARSE_PARAMETERS_NONE();
    const rapira_held *held =
        &rapira_grpc_metadata_from(Z_OBJ_P(ZEND_THIS))->held;
    RAPIRA_RETURN_THROWS_UNLESS(
        rapira_rs_grpc_snapshot(held, trailer, return_value), what);
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, headers) {
    grpc_snapshot(INTERNAL_FUNCTION_PARAM_PASSTHRU, false, "headers");
}

ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, trailers) {
    grpc_snapshot(INTERNAL_FUNCTION_PARAM_PASSTHRU, true, "trailers");
}
