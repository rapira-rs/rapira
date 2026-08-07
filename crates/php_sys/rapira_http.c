#include "rapira_classes.h"
#include "zend_API.h"
#include "zend_operators.h"
#include "zend_property_hooks.h"
#include "zend_types.h"

static void set_str_or_null(zend_class_entry *scope, zend_object *obj,
                            const char *name, size_t len, zend_string *val) {
    if (val != NULL) {
        zend_update_property_str(scope, obj, name, len, val);
    } else {
        zend_update_property_null(scope, obj, name, len);
    }
}

static bool address_arg(zval *arg, uint32_t num) {
    if (Z_TYPE_P(arg) == IS_OBJECT &&
        (instanceof_function(Z_OBJCE_P(arg), rapira_ce_inet_address) ||
         instanceof_function(Z_OBJCE_P(arg), rapira_ce_unix_address))) {
        return true;
    }
    zend_argument_type_error(
        num,
        "must be of type Rapira\\InetAddress|Rapira\\UnixAddress, %s given",
        zend_zval_value_name(arg));
    return false;
}

ZEND_METHOD(Rapira_InetAddress, __construct) {
    zend_string *ip;
    zend_long port;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(ip)
    Z_PARAM_LONG(port)
    ZEND_PARSE_PARAMETERS_END();

    zend_update_property_str(rapira_ce_inet_address, Z_OBJ_P(ZEND_THIS),
                             ZEND_STRL("ip"), ip);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_long(rapira_ce_inet_address, Z_OBJ_P(ZEND_THIS),
                              ZEND_STRL("port"), port);
}

ZEND_METHOD(Rapira_UnixAddress, __construct) {
    zend_string *path;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_STR_OR_NULL(path)
    ZEND_PARSE_PARAMETERS_END();

    set_str_or_null(rapira_ce_unix_address, Z_OBJ_P(ZEND_THIS),
                    ZEND_STRL("path"), path);
}

ZEND_METHOD(Rapira_Http_Tls, __construct) {
    zend_string *version, *cipher, *negotiated, *server_name, *serial, *org,
        *fingerprint;
    ZEND_PARSE_PARAMETERS_START(7, 7)
    Z_PARAM_STR(version)
    Z_PARAM_STR(cipher)
    Z_PARAM_STR_OR_NULL(negotiated)
    Z_PARAM_STR_OR_NULL(server_name)
    Z_PARAM_STR_OR_NULL(serial)
    Z_PARAM_STR_OR_NULL(org)
    Z_PARAM_STR_OR_NULL(fingerprint)
    ZEND_PARSE_PARAMETERS_END();

    zend_object *self = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(rapira_ce_http_tls, self, ZEND_STRL("version"),
                             version);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(rapira_ce_http_tls, self, ZEND_STRL("cipher"),
                             cipher);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(rapira_ce_http_tls, self, ZEND_STRL("negotiatedProtocol"),
                    negotiated);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(rapira_ce_http_tls, self, ZEND_STRL("requestedServerName"),
                    server_name);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(rapira_ce_http_tls, self, ZEND_STRL("certSerial"), serial);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(rapira_ce_http_tls, self, ZEND_STRL("certOrganization"),
                    org);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(rapira_ce_http_tls, self, ZEND_STRL("certFingerprint"),
                    fingerprint);
}

ZEND_METHOD(Rapira_Http_FormField, __construct) {
    zend_string *name, *value;
    zval *headers;
    ZEND_PARSE_PARAMETERS_START(3, 3)
    Z_PARAM_STR(name)
    Z_PARAM_STR(value)
    Z_PARAM_ARRAY(headers)
    ZEND_PARSE_PARAMETERS_END();

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *self = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, self, ZEND_STRL("name"), name);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(ce, self, ZEND_STRL("value"), value);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("headers"), headers);
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
    zend_object *self = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, self, ZEND_STRL("name"), name);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(ce, self, ZEND_STRL("clientFilename"),
                             client_filename);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(ce, self, ZEND_STRL("clientMediaType"), client_media_type);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("headers"), headers);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(ce, self, ZEND_STRL("tmpPath"), tmp_path);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_long(ce, self, ZEND_STRL("size"), size);
}

ZEND_METHOD(Rapira_Http_Multipart, __construct) {
    zval *fields, *files;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_ARRAY(fields)
    Z_PARAM_ARRAY(files)
    ZEND_PARSE_PARAMETERS_END();

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *self = Z_OBJ_P(ZEND_THIS);
    zend_update_property(ce, self, ZEND_STRL("fields"), fields);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("files"), files);
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
    Z_PARAM_OBJECT_OF_CLASS_OR_NULL(tls, rapira_ce_http_tls)
    Z_PARAM_DOUBLE(received_at)
    ZEND_PARSE_PARAMETERS_END();

    if (!address_arg(remote, 8) || !address_arg(server, 9)) {
        RETURN_THROWS();
    }

    zend_class_entry *ce = Z_OBJCE_P(ZEND_THIS);
    zend_object *self = Z_OBJ_P(ZEND_THIS);
    zend_update_property_str(ce, self, ZEND_STRL("method"), method);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(ce, self, ZEND_STRL("uri"), uri);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(ce, self, ZEND_STRL("target"), target);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    set_str_or_null(ce, self, ZEND_STRL("authority"), authority);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_str(ce, self, ZEND_STRL("protocol"), protocol);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("headers"), headers);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    if (body_obj != NULL) {
        zval body;
        ZVAL_OBJ(&body, body_obj);
        zend_update_property(ce, self, ZEND_STRL("body"), &body);
    } else {
        zend_update_property_str(ce, self, ZEND_STRL("body"), body_str);
    }
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("remote"), remote);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("server"), server);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property(ce, self, ZEND_STRL("tls"), tls);
    if (EG(exception)) {
        RETURN_THROWS();
    }
    zend_update_property_double(ce, self, ZEND_STRL("receivedAt"), received_at);
}
