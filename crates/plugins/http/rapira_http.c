#include "rapira_http.h"
#include "zend_API.h"
#include "zend_types.h"

// rust glue (src/php/values.rs): these return false with a PHP exception already pending
extern bool rapira_rs_ctor_form_field(zend_object *obj, zend_string *name,
                                      zend_string *value, zval *headers);
extern bool rapira_rs_ctor_uploaded_file(zend_object *obj, zend_string *name,
                                         zend_string *client_filename,
                                         zend_string *client_media_type,
                                         zval *headers, zend_string *tmp_path,
                                         int64_t size);
extern bool rapira_rs_ctor_multipart(zend_object *obj, zval *fields,
                                     zval *files);
extern bool rapira_rs_ctor_request(zend_object *obj, zend_string *method,
                                   zend_string *uri, zend_string *target,
                                   zend_string *authority,
                                   zend_string *protocol, zval *headers,
                                   zval *body, zval *remote, zval *server,
                                   zval *tls, double received_at);

ZEND_METHOD(Rapira_Http_FormField, __construct) {
    zend_string *name, *value;
    zval *headers;
    ZEND_PARSE_PARAMETERS_START(3, 3)
    Z_PARAM_STR(name)
    Z_PARAM_STR(value)
    Z_PARAM_ARRAY(headers)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_form_field(Z_OBJ_P(ZEND_THIS), name, value, headers)) {
        rapira_throw_or_backstop("FormField construction");
        RETURN_THROWS();
    }
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

    if (!rapira_rs_ctor_uploaded_file(Z_OBJ_P(ZEND_THIS), name,
                                      client_filename, client_media_type,
                                      headers, tmp_path, (int64_t)size)) {
        rapira_throw_or_backstop("UploadedFile construction");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Http_Multipart, __construct) {
    zval *fields, *files;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_ARRAY(fields)
    Z_PARAM_ARRAY(files)
    ZEND_PARSE_PARAMETERS_END();

    if (!rapira_rs_ctor_multipart(Z_OBJ_P(ZEND_THIS), fields, files)) {
        rapira_throw_or_backstop("Multipart construction");
        RETURN_THROWS();
    }
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

    // borrowed body zval: ownership stays with ZPP, no refcount taken
    zval body;
    if (body_obj != NULL) {
        ZVAL_OBJ(&body, body_obj);
    } else {
        ZVAL_STR(&body, body_str);
    }

    if (!rapira_rs_ctor_request(Z_OBJ_P(ZEND_THIS), method, uri, target,
                                authority, protocol, headers, &body, remote,
                                server, tls, received_at)) {
        rapira_throw_or_backstop("Request construction");
        RETURN_THROWS();
    }
}
