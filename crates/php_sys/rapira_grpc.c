#include "ext/spl/spl_array.h"
#include "rapira_classes.h"
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

ZEND_METHOD(Rapira_Grpc_Status, __construct) {
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
    if (!rapira_rs_ctor_grpc_status(Z_OBJ_P(ZEND_THIS), code, message,
                                    details)) {
        rapira_throw_or_backstop("Status construction");
        RETURN_THROWS();
    }
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
                                     deadline_null ? NULL : &deadline, remote,
                                     tls, protocol, received_at)) {
        rapira_throw_or_backstop("Context construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Grpc_Exception_GrpcException, __construct) {
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
    if (!rapira_rs_ctor_grpc_exception(Z_OBJ_P(ZEND_THIS), code, message,
                                       details)) {
        rapira_throw_or_backstop("GrpcException construction");
        RETURN_THROWS();
    }
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
