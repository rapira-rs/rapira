/* This is a generated file, edit the .stub.php file instead.
 * Stub hash: ea35ffe9b86137d42720daeeb8e3a05752eeef06 */

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Http_Tls___construct, 0, 0, 7)
	ZEND_ARG_TYPE_INFO(0, version, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, cipher, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, negotiatedProtocol, IS_STRING, 1)
	ZEND_ARG_TYPE_INFO(0, requestedServerName, IS_STRING, 1)
	ZEND_ARG_TYPE_INFO(0, certSerial, IS_STRING, 1)
	ZEND_ARG_TYPE_INFO(0, certOrganization, IS_STRING, 1)
	ZEND_ARG_TYPE_INFO(0, certFingerprint, IS_STRING, 1)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Http_FormField___construct, 0, 0, 3)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, value, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, headers, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Http_UploadedFile___construct, 0, 0, 6)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, clientFilename, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, clientMediaType, IS_STRING, 1)
	ZEND_ARG_TYPE_INFO(0, headers, IS_ARRAY, 0)
	ZEND_ARG_TYPE_INFO(0, tmpPath, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, size, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Http_Multipart___construct, 0, 0, 2)
	ZEND_ARG_TYPE_INFO(0, fields, IS_ARRAY, 0)
	ZEND_ARG_TYPE_INFO(0, files, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Http_Request___construct, 0, 0, 11)
	ZEND_ARG_TYPE_INFO(0, method, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, uri, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, target, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, authority, IS_STRING, 1)
	ZEND_ARG_TYPE_INFO(0, protocol, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, headers, IS_ARRAY, 0)
	ZEND_ARG_OBJ_TYPE_MASK(0, body, Rapira\\Http\\Multipart, MAY_BE_STRING, NULL)
	ZEND_ARG_OBJ_TYPE_MASK(0, remote, Rapira\\InetAddress|Rapira\\\125nixAddress, 0, NULL)
	ZEND_ARG_OBJ_TYPE_MASK(0, server, Rapira\\InetAddress|Rapira\\\125nixAddress, 0, NULL)
	ZEND_ARG_OBJ_INFO(0, tls, Rapira\\Http\\Tls, 1)
	ZEND_ARG_TYPE_INFO(0, receivedAt, IS_DOUBLE, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Http_Exchange_getRequest, 0, 0, Rapira\\Http\\Request, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Http_Exchange_writeHead, 0, 1, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, status, IS_LONG, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, headers, IS_ARRAY, 0, "[]")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Http_Exchange_writeBody, 0, 1, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, content, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, eos, _IS_BOOL, 0, "true")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Http_Exchange_sendFile, 0, 1, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, path, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, offset, IS_LONG, 0, "0")
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, length, IS_LONG, 1, "null")
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, eos, _IS_BOOL, 0, "true")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Http_Exchange_writeTrailers, 0, 1, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, trailers, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Http_Exchange_flush, 0, 0, IS_VOID, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Http_HttpDispatcher_tryReceive, 0, 0, Rapira\\Http\\Exchange, 1)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Http_HttpDispatcher_receive, 0, 0, Rapira\\Http\\Exchange, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, timeout, IS_LONG, 0, "-1")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Http_HttpDispatcher_getInfo, 0, 0, Rapira\\Http\\HttpDispatcherInfo, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Internal_Http_Dispatcher___construct, 0, 0, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Internal_Http_Dispatcher_name, 0, 0, IS_STRING, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Internal_Http_Dispatcher_tryReceive arginfo_class_Rapira_Http_HttpDispatcher_tryReceive

#define arginfo_class_Rapira_Internal_Http_Dispatcher_receive arginfo_class_Rapira_Http_HttpDispatcher_receive

#define arginfo_class_Rapira_Internal_Http_Dispatcher_getInfo arginfo_class_Rapira_Http_HttpDispatcher_getInfo

#define arginfo_class_Rapira_Internal_Http_DispatcherInfo___construct arginfo_class_Rapira_Internal_Http_Dispatcher___construct

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Internal_Http_DispatcherInfo_pendingCount, 0, 0, IS_LONG, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Internal_Http_DispatcherInfo_activeCount arginfo_class_Rapira_Internal_Http_DispatcherInfo_pendingCount

#define arginfo_class_Rapira_Internal_Http_Exchange___construct arginfo_class_Rapira_Internal_Http_Dispatcher___construct

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Internal_Http_Exchange_isFinalized, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Internal_Http_Exchange_isCancelled arginfo_class_Rapira_Internal_Http_Exchange_isFinalized

#define arginfo_class_Rapira_Internal_Http_Exchange_getRequest arginfo_class_Rapira_Http_Exchange_getRequest

#define arginfo_class_Rapira_Internal_Http_Exchange_writeHead arginfo_class_Rapira_Http_Exchange_writeHead

#define arginfo_class_Rapira_Internal_Http_Exchange_writeBody arginfo_class_Rapira_Http_Exchange_writeBody

#define arginfo_class_Rapira_Internal_Http_Exchange_sendFile arginfo_class_Rapira_Http_Exchange_sendFile

#define arginfo_class_Rapira_Internal_Http_Exchange_writeTrailers arginfo_class_Rapira_Http_Exchange_writeTrailers

#define arginfo_class_Rapira_Internal_Http_Exchange_flush arginfo_class_Rapira_Http_Exchange_flush

ZEND_METHOD(Rapira_Http_Tls, __construct);
ZEND_METHOD(Rapira_Http_FormField, __construct);
ZEND_METHOD(Rapira_Http_UploadedFile, __construct);
ZEND_METHOD(Rapira_Http_Multipart, __construct);
ZEND_METHOD(Rapira_Http_Request, __construct);
ZEND_METHOD(Rapira_Internal_Http_Dispatcher, __construct);
ZEND_METHOD(Rapira_Internal_Http_Dispatcher, name);
ZEND_METHOD(Rapira_Internal_Http_Dispatcher, tryReceive);
ZEND_METHOD(Rapira_Internal_Http_Dispatcher, receive);
ZEND_METHOD(Rapira_Internal_Http_Dispatcher, getInfo);
ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, __construct);
ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, pendingCount);
ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, activeCount);
ZEND_METHOD(Rapira_Internal_Http_Exchange, __construct);
ZEND_METHOD(Rapira_Internal_Http_Exchange, isFinalized);
ZEND_METHOD(Rapira_Internal_Http_Exchange, isCancelled);
ZEND_METHOD(Rapira_Internal_Http_Exchange, getRequest);
ZEND_METHOD(Rapira_Internal_Http_Exchange, writeHead);
ZEND_METHOD(Rapira_Internal_Http_Exchange, writeBody);
ZEND_METHOD(Rapira_Internal_Http_Exchange, sendFile);
ZEND_METHOD(Rapira_Internal_Http_Exchange, writeTrailers);
ZEND_METHOD(Rapira_Internal_Http_Exchange, flush);

static const zend_function_entry class_Rapira_Http_Tls_methods[] = {
	ZEND_ME(Rapira_Http_Tls, __construct, arginfo_class_Rapira_Http_Tls___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Http_FormField_methods[] = {
	ZEND_ME(Rapira_Http_FormField, __construct, arginfo_class_Rapira_Http_FormField___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Http_UploadedFile_methods[] = {
	ZEND_ME(Rapira_Http_UploadedFile, __construct, arginfo_class_Rapira_Http_UploadedFile___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Http_Multipart_methods[] = {
	ZEND_ME(Rapira_Http_Multipart, __construct, arginfo_class_Rapira_Http_Multipart___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Http_Request_methods[] = {
	ZEND_ME(Rapira_Http_Request, __construct, arginfo_class_Rapira_Http_Request___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Http_Exchange_methods[] = {
	ZEND_RAW_FENTRY("getRequest", NULL, arginfo_class_Rapira_Http_Exchange_getRequest, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("writeHead", NULL, arginfo_class_Rapira_Http_Exchange_writeHead, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("writeBody", NULL, arginfo_class_Rapira_Http_Exchange_writeBody, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("sendFile", NULL, arginfo_class_Rapira_Http_Exchange_sendFile, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("writeTrailers", NULL, arginfo_class_Rapira_Http_Exchange_writeTrailers, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("flush", NULL, arginfo_class_Rapira_Http_Exchange_flush, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Http_HttpDispatcher_methods[] = {
	ZEND_RAW_FENTRY("tryReceive", NULL, arginfo_class_Rapira_Http_HttpDispatcher_tryReceive, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("receive", NULL, arginfo_class_Rapira_Http_HttpDispatcher_receive, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("getInfo", NULL, arginfo_class_Rapira_Http_HttpDispatcher_getInfo, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Http_Dispatcher_methods[] = {
	ZEND_ME(Rapira_Internal_Http_Dispatcher, __construct, arginfo_class_Rapira_Internal_Http_Dispatcher___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Http_Dispatcher, name, arginfo_class_Rapira_Internal_Http_Dispatcher_name, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Dispatcher, tryReceive, arginfo_class_Rapira_Internal_Http_Dispatcher_tryReceive, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Dispatcher, receive, arginfo_class_Rapira_Internal_Http_Dispatcher_receive, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Dispatcher, getInfo, arginfo_class_Rapira_Internal_Http_Dispatcher_getInfo, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Http_DispatcherInfo_methods[] = {
	ZEND_ME(Rapira_Internal_Http_DispatcherInfo, __construct, arginfo_class_Rapira_Internal_Http_DispatcherInfo___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Http_DispatcherInfo, pendingCount, arginfo_class_Rapira_Internal_Http_DispatcherInfo_pendingCount, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_DispatcherInfo, activeCount, arginfo_class_Rapira_Internal_Http_DispatcherInfo_activeCount, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Http_Exchange_methods[] = {
	ZEND_ME(Rapira_Internal_Http_Exchange, __construct, arginfo_class_Rapira_Internal_Http_Exchange___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Http_Exchange, isFinalized, arginfo_class_Rapira_Internal_Http_Exchange_isFinalized, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, isCancelled, arginfo_class_Rapira_Internal_Http_Exchange_isCancelled, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, getRequest, arginfo_class_Rapira_Internal_Http_Exchange_getRequest, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, writeHead, arginfo_class_Rapira_Internal_Http_Exchange_writeHead, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, writeBody, arginfo_class_Rapira_Internal_Http_Exchange_writeBody, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, sendFile, arginfo_class_Rapira_Internal_Http_Exchange_sendFile, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, writeTrailers, arginfo_class_Rapira_Internal_Http_Exchange_writeTrailers, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Http_Exchange, flush, arginfo_class_Rapira_Internal_Http_Exchange_flush, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static zend_class_entry *register_class_Rapira_Http_Tls(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "Tls", class_Rapira_Http_Tls_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_version_default_value;
	ZVAL_UNDEF(&property_version_default_value);
	zend_string *property_version_name = zend_string_init("version", sizeof("version") - 1, 1);
	zend_declare_typed_property(class_entry, property_version_name, &property_version_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_version_name);

	zval property_cipher_default_value;
	ZVAL_UNDEF(&property_cipher_default_value);
	zend_string *property_cipher_name = zend_string_init("cipher", sizeof("cipher") - 1, 1);
	zend_declare_typed_property(class_entry, property_cipher_name, &property_cipher_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_cipher_name);

	zval property_negotiatedProtocol_default_value;
	ZVAL_UNDEF(&property_negotiatedProtocol_default_value);
	zend_string *property_negotiatedProtocol_name = zend_string_init("negotiatedProtocol", sizeof("negotiatedProtocol") - 1, 1);
	zend_declare_typed_property(class_entry, property_negotiatedProtocol_name, &property_negotiatedProtocol_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_negotiatedProtocol_name);

	zval property_requestedServerName_default_value;
	ZVAL_UNDEF(&property_requestedServerName_default_value);
	zend_string *property_requestedServerName_name = zend_string_init("requestedServerName", sizeof("requestedServerName") - 1, 1);
	zend_declare_typed_property(class_entry, property_requestedServerName_name, &property_requestedServerName_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_requestedServerName_name);

	zval property_certSerial_default_value;
	ZVAL_UNDEF(&property_certSerial_default_value);
	zend_string *property_certSerial_name = zend_string_init("certSerial", sizeof("certSerial") - 1, 1);
	zend_declare_typed_property(class_entry, property_certSerial_name, &property_certSerial_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_certSerial_name);

	zval property_certOrganization_default_value;
	ZVAL_UNDEF(&property_certOrganization_default_value);
	zend_string *property_certOrganization_name = zend_string_init("certOrganization", sizeof("certOrganization") - 1, 1);
	zend_declare_typed_property(class_entry, property_certOrganization_name, &property_certOrganization_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_certOrganization_name);

	zval property_certFingerprint_default_value;
	ZVAL_UNDEF(&property_certFingerprint_default_value);
	zend_string *property_certFingerprint_name = zend_string_init("certFingerprint", sizeof("certFingerprint") - 1, 1);
	zend_declare_typed_property(class_entry, property_certFingerprint_name, &property_certFingerprint_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_certFingerprint_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_FormField(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "FormField", class_Rapira_Http_FormField_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_name_default_value;
	ZVAL_UNDEF(&property_name_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_NAME), &property_name_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_value_default_value;
	ZVAL_UNDEF(&property_value_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_VALUE), &property_value_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_headers_default_value;
	ZVAL_UNDEF(&property_headers_default_value);
	zend_string *property_headers_name = zend_string_init("headers", sizeof("headers") - 1, 1);
	zend_declare_typed_property(class_entry, property_headers_name, &property_headers_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_headers_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_UploadedFile(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "UploadedFile", class_Rapira_Http_UploadedFile_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_name_default_value;
	ZVAL_UNDEF(&property_name_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_NAME), &property_name_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_clientFilename_default_value;
	ZVAL_UNDEF(&property_clientFilename_default_value);
	zend_string *property_clientFilename_name = zend_string_init("clientFilename", sizeof("clientFilename") - 1, 1);
	zend_declare_typed_property(class_entry, property_clientFilename_name, &property_clientFilename_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_clientFilename_name);

	zval property_clientMediaType_default_value;
	ZVAL_UNDEF(&property_clientMediaType_default_value);
	zend_string *property_clientMediaType_name = zend_string_init("clientMediaType", sizeof("clientMediaType") - 1, 1);
	zend_declare_typed_property(class_entry, property_clientMediaType_name, &property_clientMediaType_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_clientMediaType_name);

	zval property_headers_default_value;
	ZVAL_UNDEF(&property_headers_default_value);
	zend_string *property_headers_name = zend_string_init("headers", sizeof("headers") - 1, 1);
	zend_declare_typed_property(class_entry, property_headers_name, &property_headers_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_headers_name);

	zval property_tmpPath_default_value;
	ZVAL_UNDEF(&property_tmpPath_default_value);
	zend_string *property_tmpPath_name = zend_string_init("tmpPath", sizeof("tmpPath") - 1, 1);
	zend_declare_typed_property(class_entry, property_tmpPath_name, &property_tmpPath_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_tmpPath_name);

	zval property_size_default_value;
	ZVAL_UNDEF(&property_size_default_value);
	zend_string *property_size_name = zend_string_init("size", sizeof("size") - 1, 1);
	zend_declare_typed_property(class_entry, property_size_name, &property_size_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_size_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Multipart(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "Multipart", class_Rapira_Http_Multipart_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_fields_default_value;
	ZVAL_UNDEF(&property_fields_default_value);
	zend_string *property_fields_name = zend_string_init("fields", sizeof("fields") - 1, 1);
	zend_declare_typed_property(class_entry, property_fields_name, &property_fields_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_fields_name);

	zval property_files_default_value;
	ZVAL_UNDEF(&property_files_default_value);
	zend_string *property_files_name = zend_string_init("files", sizeof("files") - 1, 1);
	zend_declare_typed_property(class_entry, property_files_name, &property_files_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_files_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Request(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "Request", class_Rapira_Http_Request_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_method_default_value;
	ZVAL_UNDEF(&property_method_default_value);
	zend_string *property_method_name = zend_string_init("method", sizeof("method") - 1, 1);
	zend_declare_typed_property(class_entry, property_method_name, &property_method_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_method_name);

	zval property_uri_default_value;
	ZVAL_UNDEF(&property_uri_default_value);
	zend_string *property_uri_name = zend_string_init("uri", sizeof("uri") - 1, 1);
	zend_declare_typed_property(class_entry, property_uri_name, &property_uri_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_uri_name);

	zval property_target_default_value;
	ZVAL_UNDEF(&property_target_default_value);
	zend_string *property_target_name = zend_string_init("target", sizeof("target") - 1, 1);
	zend_declare_typed_property(class_entry, property_target_name, &property_target_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_target_name);

	zval property_authority_default_value;
	ZVAL_UNDEF(&property_authority_default_value);
	zend_string *property_authority_name = zend_string_init("authority", sizeof("authority") - 1, 1);
	zend_declare_typed_property(class_entry, property_authority_name, &property_authority_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));
	zend_string_release(property_authority_name);

	zval property_protocol_default_value;
	ZVAL_UNDEF(&property_protocol_default_value);
	zend_string *property_protocol_name = zend_string_init("protocol", sizeof("protocol") - 1, 1);
	zend_declare_typed_property(class_entry, property_protocol_name, &property_protocol_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_protocol_name);

	zval property_headers_default_value;
	ZVAL_UNDEF(&property_headers_default_value);
	zend_string *property_headers_name = zend_string_init("headers", sizeof("headers") - 1, 1);
	zend_declare_typed_property(class_entry, property_headers_name, &property_headers_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_headers_name);

	zval property_body_default_value;
	ZVAL_UNDEF(&property_body_default_value);
	zend_string *property_body_name = zend_string_init("body", sizeof("body") - 1, 1);
	zend_string *property_body_class_Rapira_Http_Multipart = zend_string_init("Rapira\\Http\\Multipart", sizeof("Rapira\\Http\\Multipart")-1, 1);
	zend_declare_typed_property(class_entry, property_body_name, &property_body_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_body_class_Rapira_Http_Multipart, 0, MAY_BE_STRING));
	zend_string_release(property_body_name);

	zval property_remote_default_value;
	ZVAL_UNDEF(&property_remote_default_value);
	zend_string *property_remote_name = zend_string_init("remote", sizeof("remote") - 1, 1);
	zend_string *property_remote_class_Rapira_InetAddress = zend_string_init("Rapira\\InetAddress", sizeof("Rapira\\InetAddress") - 1, 1);
	zend_string *property_remote_class_Rapira_UnixAddress = zend_string_init("Rapira\\\125nixAddress", sizeof("Rapira\\\125nixAddress") - 1, 1);
	zend_type_list *property_remote_type_list = malloc(ZEND_TYPE_LIST_SIZE(2));
	property_remote_type_list->num_types = 2;
	property_remote_type_list->types[0] = (zend_type) ZEND_TYPE_INIT_CLASS(property_remote_class_Rapira_InetAddress, 0, 0);
	property_remote_type_list->types[1] = (zend_type) ZEND_TYPE_INIT_CLASS(property_remote_class_Rapira_UnixAddress, 0, 0);
	zend_type property_remote_type = ZEND_TYPE_INIT_UNION(property_remote_type_list, 0);
	zend_declare_typed_property(class_entry, property_remote_name, &property_remote_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, property_remote_type);
	zend_string_release(property_remote_name);

	zval property_server_default_value;
	ZVAL_UNDEF(&property_server_default_value);
	zend_string *property_server_name = zend_string_init("server", sizeof("server") - 1, 1);
	zend_string *property_server_class_Rapira_InetAddress = zend_string_init("Rapira\\InetAddress", sizeof("Rapira\\InetAddress") - 1, 1);
	zend_string *property_server_class_Rapira_UnixAddress = zend_string_init("Rapira\\\125nixAddress", sizeof("Rapira\\\125nixAddress") - 1, 1);
	zend_type_list *property_server_type_list = malloc(ZEND_TYPE_LIST_SIZE(2));
	property_server_type_list->num_types = 2;
	property_server_type_list->types[0] = (zend_type) ZEND_TYPE_INIT_CLASS(property_server_class_Rapira_InetAddress, 0, 0);
	property_server_type_list->types[1] = (zend_type) ZEND_TYPE_INIT_CLASS(property_server_class_Rapira_UnixAddress, 0, 0);
	zend_type property_server_type = ZEND_TYPE_INIT_UNION(property_server_type_list, 0);
	zend_declare_typed_property(class_entry, property_server_name, &property_server_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, property_server_type);
	zend_string_release(property_server_name);

	zval property_tls_default_value;
	ZVAL_UNDEF(&property_tls_default_value);
	zend_string *property_tls_name = zend_string_init("tls", sizeof("tls") - 1, 1);
	zend_string *property_tls_class_Rapira_Http_Tls = zend_string_init("Rapira\\Http\\Tls", sizeof("Rapira\\Http\\Tls")-1, 1);
	zend_declare_typed_property(class_entry, property_tls_name, &property_tls_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_tls_class_Rapira_Http_Tls, 0, MAY_BE_NULL));
	zend_string_release(property_tls_name);

	zval property_receivedAt_default_value;
	ZVAL_UNDEF(&property_receivedAt_default_value);
	zend_string *property_receivedAt_name = zend_string_init("receivedAt", sizeof("receivedAt") - 1, 1);
	zend_declare_typed_property(class_entry, property_receivedAt_name, &property_receivedAt_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_DOUBLE));
	zend_string_release(property_receivedAt_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_HttpDispatcherInfo(zend_class_entry *class_entry_Rapira_DispatcherInfo)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "HttpDispatcherInfo", NULL);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_DispatcherInfo);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Exchange(zend_class_entry *class_entry_Rapira_Work)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "Exchange", class_Rapira_Http_Exchange_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Work);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_HttpDispatcher(zend_class_entry *class_entry_Rapira_Dispatcher)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http", "HttpDispatcher", class_Rapira_Http_HttpDispatcher_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Dispatcher);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Exception_ContentLengthExceededError(zend_class_entry *class_entry_Error, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http\\Exception", "ContentLengthExceededError", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Error, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Exception_HeadAlreadyWrittenError(zend_class_entry *class_entry_Error, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http\\Exception", "HeadAlreadyWrittenError", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Error, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Exception_HeadNotWrittenError(zend_class_entry *class_entry_Error, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http\\Exception", "HeadNotWrittenError", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Error, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Http_Exception_FileNotSendableException(zend_class_entry *class_entry_RuntimeException, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Http\\Exception", "FileNotSendableException", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_RuntimeException, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Http_Dispatcher(zend_class_entry *class_entry_Rapira_Http_HttpDispatcher)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Http", "Dispatcher", class_Rapira_Internal_Http_Dispatcher_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Http_HttpDispatcher);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Http_DispatcherInfo(zend_class_entry *class_entry_Rapira_Http_HttpDispatcherInfo)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Http", "DispatcherInfo", class_Rapira_Internal_Http_DispatcherInfo_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Http_HttpDispatcherInfo);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Http_Exchange(zend_class_entry *class_entry_Rapira_Http_Exchange)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Http", "Exchange", class_Rapira_Internal_Http_Exchange_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Http_Exchange);

	return class_entry;
}
