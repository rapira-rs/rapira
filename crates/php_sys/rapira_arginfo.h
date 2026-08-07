/* This is a generated file, edit the .stub.php file instead.
 * Stub hash: 51d3551335436535a6ac9df2f62b562a8830ff6c */

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_finish_request, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_Rapira_get_dispatcher, 0, 0, Rapira\\Dispatcher, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_Rapira_get_version, 0, 0, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_Rapira_log, 0, 1, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, message, IS_STRING, 0)
	ZEND_ARG_OBJ_INFO_WITH_DEFAULT_VALUE(0, level, Rapira\\LogLevel, 0, "Rapira\\LogLevel::Info")
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, context, IS_ARRAY, 0, "[]")
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Work_isFinalized arginfo_rapira_finish_request

#define arginfo_class_Rapira_Work_isCancelled arginfo_rapira_finish_request

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_DispatcherInfo_pendingCount, 0, 0, IS_LONG, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_DispatcherInfo_activeCount arginfo_class_Rapira_DispatcherInfo_pendingCount

#define arginfo_class_Rapira_Dispatcher_name arginfo_Rapira_get_version

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Dispatcher_tryReceive, 0, 0, Rapira\\Work, 1)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Dispatcher_receive, 0, 0, Rapira\\Work, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, timeout, IS_LONG, 0, "-1")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Dispatcher_getInfo, 0, 0, Rapira\\DispatcherInfo, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_InetAddress___construct, 0, 0, 2)
	ZEND_ARG_TYPE_INFO(0, ip, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, port, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_UnixAddress___construct, 0, 0, 1)
	ZEND_ARG_TYPE_INFO(0, path, IS_STRING, 1)
ZEND_END_ARG_INFO()

ZEND_FUNCTION(rapira_finish_request);
ZEND_FUNCTION(Rapira_get_dispatcher);
ZEND_FUNCTION(Rapira_get_version);
ZEND_FUNCTION(Rapira_log);
ZEND_METHOD(Rapira_InetAddress, __construct);
ZEND_METHOD(Rapira_UnixAddress, __construct);

static const zend_function_entry ext_functions[] = {
	ZEND_FE(rapira_finish_request, arginfo_rapira_finish_request)
	ZEND_RAW_FENTRY(ZEND_NS_NAME("Rapira", "get_dispatcher"), zif_Rapira_get_dispatcher, arginfo_Rapira_get_dispatcher, 0, NULL, NULL)
	ZEND_RAW_FENTRY(ZEND_NS_NAME("Rapira", "get_version"), zif_Rapira_get_version, arginfo_Rapira_get_version, 0, NULL, NULL)
	ZEND_RAW_FENTRY(ZEND_NS_NAME("Rapira", "log"), zif_Rapira_log, arginfo_Rapira_log, 0, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Work_methods[] = {
	ZEND_RAW_FENTRY("isFinalized", NULL, arginfo_class_Rapira_Work_isFinalized, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("isCancelled", NULL, arginfo_class_Rapira_Work_isCancelled, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_DispatcherInfo_methods[] = {
	ZEND_RAW_FENTRY("pendingCount", NULL, arginfo_class_Rapira_DispatcherInfo_pendingCount, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("activeCount", NULL, arginfo_class_Rapira_DispatcherInfo_activeCount, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Dispatcher_methods[] = {
	ZEND_RAW_FENTRY("name", NULL, arginfo_class_Rapira_Dispatcher_name, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("tryReceive", NULL, arginfo_class_Rapira_Dispatcher_tryReceive, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("receive", NULL, arginfo_class_Rapira_Dispatcher_receive, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("getInfo", NULL, arginfo_class_Rapira_Dispatcher_getInfo, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_InetAddress_methods[] = {
	ZEND_ME(Rapira_InetAddress, __construct, arginfo_class_Rapira_InetAddress___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_UnixAddress_methods[] = {
	ZEND_ME(Rapira_UnixAddress, __construct, arginfo_class_Rapira_UnixAddress___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static zend_class_entry *register_class_Rapira_LogLevel(void)
{
	zend_class_entry *class_entry = zend_register_internal_enum("Rapira\\LogLevel", IS_UNDEF, NULL);

	zend_enum_add_case_cstr(class_entry, "Error", NULL);

	zend_enum_add_case_cstr(class_entry, "Warning", NULL);

	zend_enum_add_case_cstr(class_entry, "Info", NULL);

	zend_enum_add_case_cstr(class_entry, "Debug", NULL);

	zend_enum_add_case_cstr(class_entry, "Trace", NULL);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Work(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "Work", class_Rapira_Work_methods);
	class_entry = zend_register_internal_interface(&ce);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_DispatcherInfo(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "DispatcherInfo", class_Rapira_DispatcherInfo_methods);
	class_entry = zend_register_internal_interface(&ce);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Dispatcher(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "Dispatcher", class_Rapira_Dispatcher_methods);
	class_entry = zend_register_internal_interface(&ce);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_InetAddress(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "InetAddress", class_Rapira_InetAddress_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_ip_default_value;
	ZVAL_UNDEF(&property_ip_default_value);
	zend_string *property_ip_name = zend_string_init("ip", sizeof("ip") - 1, 1);
	zend_declare_typed_property(class_entry, property_ip_name, &property_ip_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_ip_name);

	zval property_port_default_value;
	ZVAL_UNDEF(&property_port_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_PORT), &property_port_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));

	return class_entry;
}

static zend_class_entry *register_class_Rapira_UnixAddress(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "UnixAddress", class_Rapira_UnixAddress_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_path_default_value;
	ZVAL_UNDEF(&property_path_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_PATH), &property_path_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING|MAY_BE_NULL));

	return class_entry;
}
