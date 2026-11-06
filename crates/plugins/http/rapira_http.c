#include "rapira_http.h"

ZEND_METHOD(Rapira_Http_FormField, __construct) {
    zend_string *name, *value;
    zval *headers;
    ZEND_PARSE_PARAMETERS_START(3, 3)
    Z_PARAM_STR(name)
    Z_PARAM_STR(value)
    Z_PARAM_ARRAY(headers)
    ZEND_PARSE_PARAMETERS_END();

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, obj, ZEND_STRL("name"), name);
    zend_update_property_str(ce, obj, ZEND_STRL("value"), value);
    zend_update_property(ce, obj, ZEND_STRL("headers"), headers);
}

ZEND_METHOD(Rapira_Http_UploadedFile, __construct) {
    zend_string *name, *client_filename, *client_media_type, *tmp_path;
    zval *headers;
    zend_long size;
    ZEND_PARSE_PARAMETERS_START(6, 6)
    Z_PARAM_STR(name)
    Z_PARAM_STR(client_filename)
    Z_PARAM_STR_OR_NULL(client_media_type)
    Z_PARAM_ARRAY(headers)
    Z_PARAM_STR(tmp_path)
    Z_PARAM_LONG(size)
    ZEND_PARSE_PARAMETERS_END();

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, obj, ZEND_STRL("name"), name);
    zend_update_property_str(ce, obj, ZEND_STRL("clientFilename"),
                             client_filename);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("clientMediaType"),
                            client_media_type);
    zend_update_property(ce, obj, ZEND_STRL("headers"), headers);
    zend_update_property_str(ce, obj, ZEND_STRL("tmpPath"), tmp_path);
    zend_update_property_long(ce, obj, ZEND_STRL("size"), size);
}

ZEND_METHOD(Rapira_Http_Multipart, __construct) {
    zval *fields, *files;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_ARRAY(fields)
    Z_PARAM_ARRAY(files)
    ZEND_PARSE_PARAMETERS_END();

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property(ce, obj, ZEND_STRL("fields"), fields);
    zend_update_property(ce, obj, ZEND_STRL("files"), files);
}

ZEND_METHOD(Rapira_Http_Request, __construct) {
    zend_string *method, *uri, *target, *authority, *protocol, *body_str;
    zend_object *body_obj;
    zval *headers, *remote, *server, *tls;
    double received_at;
    ZEND_PARSE_PARAMETERS_START(11, 11)
    Z_PARAM_STR(method)
    Z_PARAM_STR(uri)
    Z_PARAM_STR(target)
    Z_PARAM_STR_OR_NULL(authority)
    Z_PARAM_STR(protocol)
    Z_PARAM_ARRAY(headers)
    Z_PARAM_OBJ_OF_CLASS_OR_STR(body_obj, rapira_ce_http_multipart, body_str)
    Z_PARAM_ZVAL(remote)
    Z_PARAM_ZVAL(server)
    Z_PARAM_OBJECT_OF_CLASS_OR_NULL(tls, rapira_ce_tls)
    Z_PARAM_DOUBLE(received_at)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_address_arg(remote, 8) || !rapira_address_arg(server, 9)) {
        RETURN_THROWS();
    }

    // borrowed body zval: ownership stays with ZPP, no refcount taken
    zval body;
    if (body_obj != NULL) {
        ZVAL_OBJ(&body, body_obj);
    } else {
        ZVAL_STR(&body, body_str);
    }

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *obj = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, obj, ZEND_STRL("method"), method);
    zend_update_property_str(ce, obj, ZEND_STRL("uri"), uri);
    zend_update_property_str(ce, obj, ZEND_STRL("target"), target);
    rapira_prop_str_or_null(ce, obj, ZEND_STRL("authority"), authority);
    zend_update_property_str(ce, obj, ZEND_STRL("protocol"), protocol);
    zend_update_property(ce, obj, ZEND_STRL("headers"), headers);
    zend_update_property(ce, obj, ZEND_STRL("body"), &body);
    zend_update_property(ce, obj, ZEND_STRL("remote"), remote);
    zend_update_property(ce, obj, ZEND_STRL("server"), server);
    if (tls) {
        zend_update_property(ce, obj, ZEND_STRL("tls"), tls);
    } else {
        zend_update_property_null(ce, obj, ZEND_STRL("tls"));
    }
    zend_update_property_double(ce, obj, ZEND_STRL("receivedAt"), received_at);
}
