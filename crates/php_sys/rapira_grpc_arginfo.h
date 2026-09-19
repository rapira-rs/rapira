/* This is a generated file, edit the .stub.php file instead.
 * Stub hash: d09de2ee837bbae87e117512eb52368b2b01613e */

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_GrpcDispatcher_tryReceive, 0, 0, Rapira\\Grpc\\\125naryCall, 1)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_GrpcDispatcher_receive, 0, 0, Rapira\\Grpc\\\125naryCall, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, timeout, IS_LONG, 0, "-1")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_GrpcDispatcher_getInfo, 0, 0, Rapira\\Grpc\\GrpcDispatcherInfo, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_GrpcDispatcher_getServices, 0, 0, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_Call_getContext, 0, 0, Rapira\\Grpc\\Call\\Context, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_Responder_getResponseMetadata, 0, 0, Rapira\\Grpc\\Responder\\ResponseMetadata, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_Responder_fail, 0, 1, IS_VOID, 0)
	ZEND_ARG_OBJ_INFO(0, status, Rapira\\Grpc\\Status, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_UnaryRequest_getMessage, 0, 0, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_UnaryResponder_respond, 0, 1, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, message, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Grpc_Metadata___construct, 0, 0, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, entries, IS_ARRAY, 0, "[]")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_Metadata_values, 0, 1, IS_ARRAY, 0)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_Metadata_count, 0, 0, IS_LONG, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_Metadata_getIterator, 0, 0, Iterator, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_MethodKind_isStreamingRequest, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Grpc_MethodKind_isStreamingResponse arginfo_class_Rapira_Grpc_MethodKind_isStreamingRequest

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Grpc_ErrorDetail___construct, 0, 0, 2)
	ZEND_ARG_TYPE_INFO(0, typeUrl, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, value, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Grpc_Status___construct, 0, 0, 1)
	ZEND_ARG_OBJ_INFO(0, code, Rapira\\Grpc\\StatusCode, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, message, IS_STRING, 0, "\'\'")
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, details, IS_ARRAY, 0, "[]")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Grpc_MethodInfo___construct, 0, 0, 4)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, inputType, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, outputType, IS_STRING, 0)
	ZEND_ARG_OBJ_INFO(0, kind, Rapira\\Grpc\\MethodKind, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Grpc_ServiceInfo___construct, 0, 0, 2)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, methods, IS_ARRAY, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Grpc_Call_Context___construct, 0, 0, 7)
	ZEND_ARG_TYPE_INFO(0, method, IS_STRING, 0)
	ZEND_ARG_OBJ_INFO(0, metadata, Rapira\\Grpc\\Metadata, 0)
	ZEND_ARG_TYPE_INFO(0, deadline, IS_DOUBLE, 1)
	ZEND_ARG_OBJ_TYPE_MASK(0, remote, Rapira\\InetAddress|Rapira\\\125nixAddress, 0, NULL)
	ZEND_ARG_OBJ_INFO(0, tls, Rapira\\Tls, 1)
	ZEND_ARG_OBJ_INFO(0, protocol, Rapira\\Grpc\\Call\\Protocol, 0)
	ZEND_ARG_TYPE_INFO(0, receivedAt, IS_DOUBLE, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addHeader, 0, 2, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, value, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryHeader, 0, 2, IS_VOID, 0)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, bytes, IS_STRING, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addTrailer arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addHeader

#define arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryTrailer arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryHeader

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_headers, 0, 0, Rapira\\Grpc\\Metadata, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_trailers arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_headers

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Internal_Grpc_Dispatcher___construct, 0, 0, 0)
ZEND_END_ARG_INFO()

#define arginfo_class_Rapira_Internal_Grpc_Dispatcher_name arginfo_class_Rapira_Grpc_UnaryRequest_getMessage

#define arginfo_class_Rapira_Internal_Grpc_Dispatcher_tryReceive arginfo_class_Rapira_Grpc_GrpcDispatcher_tryReceive

#define arginfo_class_Rapira_Internal_Grpc_Dispatcher_receive arginfo_class_Rapira_Grpc_GrpcDispatcher_receive

#define arginfo_class_Rapira_Internal_Grpc_Dispatcher_getInfo arginfo_class_Rapira_Grpc_GrpcDispatcher_getInfo

#define arginfo_class_Rapira_Internal_Grpc_Dispatcher_getServices arginfo_class_Rapira_Grpc_GrpcDispatcher_getServices

#define arginfo_class_Rapira_Internal_Grpc_DispatcherInfo___construct arginfo_class_Rapira_Internal_Grpc_Dispatcher___construct

#define arginfo_class_Rapira_Internal_Grpc_DispatcherInfo_pendingCount arginfo_class_Rapira_Grpc_Metadata_count

#define arginfo_class_Rapira_Internal_Grpc_DispatcherInfo_activeCount arginfo_class_Rapira_Grpc_Metadata_count

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall___construct arginfo_class_Rapira_Internal_Grpc_Dispatcher___construct

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_isFinalized arginfo_class_Rapira_Grpc_MethodKind_isStreamingRequest

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_isCancelled arginfo_class_Rapira_Grpc_MethodKind_isStreamingRequest

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall___destruct arginfo_class_Rapira_Internal_Grpc_Dispatcher___construct

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_getContext arginfo_class_Rapira_Grpc_Call_getContext

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_getMessage arginfo_class_Rapira_Grpc_UnaryRequest_getMessage

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_getResponseMetadata arginfo_class_Rapira_Grpc_Responder_getResponseMetadata

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_respond arginfo_class_Rapira_Grpc_UnaryResponder_respond

#define arginfo_class_Rapira_Internal_Grpc_UnaryCall_fail arginfo_class_Rapira_Grpc_Responder_fail

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata___construct arginfo_class_Rapira_Internal_Grpc_Dispatcher___construct

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addHeader arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addHeader

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addBinaryHeader arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryHeader

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addTrailer arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addHeader

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addBinaryTrailer arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryHeader

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_headers arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_headers

#define arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_trailers arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_headers

#define arginfo_class_Rapira_Grpc_Exception_GrpcException___construct arginfo_class_Rapira_Grpc_Status___construct

ZEND_METHOD(Rapira_Grpc_Metadata, __construct);
ZEND_METHOD(Rapira_Grpc_Metadata, values);
ZEND_METHOD(Rapira_Grpc_Metadata, count);
ZEND_METHOD(Rapira_Grpc_Metadata, getIterator);
ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingRequest);
ZEND_METHOD(Rapira_Grpc_MethodKind, isStreamingResponse);
ZEND_METHOD(Rapira_Grpc_ErrorDetail, __construct);
ZEND_METHOD(Rapira_Grpc_Status, __construct);
ZEND_METHOD(Rapira_Grpc_MethodInfo, __construct);
ZEND_METHOD(Rapira_Grpc_ServiceInfo, __construct);
ZEND_METHOD(Rapira_Grpc_Call_Context, __construct);
ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, __construct);
ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, name);
ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, tryReceive);
ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, receive);
ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, getInfo);
ZEND_METHOD(Rapira_Internal_Grpc_Dispatcher, getServices);
ZEND_METHOD(Rapira_Internal_Grpc_DispatcherInfo, __construct);
ZEND_METHOD(Rapira_Internal_Grpc_DispatcherInfo, pendingCount);
ZEND_METHOD(Rapira_Internal_Grpc_DispatcherInfo, activeCount);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, __construct);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, isFinalized);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, isCancelled);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, __destruct);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getContext);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getMessage);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, getResponseMetadata);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, respond);
ZEND_METHOD(Rapira_Internal_Grpc_UnaryCall, fail);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, __construct);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addHeader);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addBinaryHeader);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addTrailer);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, addBinaryTrailer);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, headers);
ZEND_METHOD(Rapira_Internal_Grpc_ResponseMetadata, trailers);
ZEND_METHOD(Rapira_Grpc_Exception_GrpcException, __construct);

static const zend_function_entry class_Rapira_Grpc_GrpcDispatcher_methods[] = {
	ZEND_RAW_FENTRY("tryReceive", NULL, arginfo_class_Rapira_Grpc_GrpcDispatcher_tryReceive, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("receive", NULL, arginfo_class_Rapira_Grpc_GrpcDispatcher_receive, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("getInfo", NULL, arginfo_class_Rapira_Grpc_GrpcDispatcher_getInfo, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("getServices", NULL, arginfo_class_Rapira_Grpc_GrpcDispatcher_getServices, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Call_methods[] = {
	ZEND_RAW_FENTRY("getContext", NULL, arginfo_class_Rapira_Grpc_Call_getContext, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Responder_methods[] = {
	ZEND_RAW_FENTRY("getResponseMetadata", NULL, arginfo_class_Rapira_Grpc_Responder_getResponseMetadata, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("fail", NULL, arginfo_class_Rapira_Grpc_Responder_fail, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_UnaryRequest_methods[] = {
	ZEND_RAW_FENTRY("getMessage", NULL, arginfo_class_Rapira_Grpc_UnaryRequest_getMessage, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_UnaryResponder_methods[] = {
	ZEND_RAW_FENTRY("respond", NULL, arginfo_class_Rapira_Grpc_UnaryResponder_respond, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Metadata_methods[] = {
	ZEND_ME(Rapira_Grpc_Metadata, __construct, arginfo_class_Rapira_Grpc_Metadata___construct, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Grpc_Metadata, values, arginfo_class_Rapira_Grpc_Metadata_values, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Grpc_Metadata, count, arginfo_class_Rapira_Grpc_Metadata_count, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Grpc_Metadata, getIterator, arginfo_class_Rapira_Grpc_Metadata_getIterator, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_MethodKind_methods[] = {
	ZEND_ME(Rapira_Grpc_MethodKind, isStreamingRequest, arginfo_class_Rapira_Grpc_MethodKind_isStreamingRequest, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Grpc_MethodKind, isStreamingResponse, arginfo_class_Rapira_Grpc_MethodKind_isStreamingResponse, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_ErrorDetail_methods[] = {
	ZEND_ME(Rapira_Grpc_ErrorDetail, __construct, arginfo_class_Rapira_Grpc_ErrorDetail___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Status_methods[] = {
	ZEND_ME(Rapira_Grpc_Status, __construct, arginfo_class_Rapira_Grpc_Status___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_MethodInfo_methods[] = {
	ZEND_ME(Rapira_Grpc_MethodInfo, __construct, arginfo_class_Rapira_Grpc_MethodInfo___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_ServiceInfo_methods[] = {
	ZEND_ME(Rapira_Grpc_ServiceInfo, __construct, arginfo_class_Rapira_Grpc_ServiceInfo___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Call_Context_methods[] = {
	ZEND_ME(Rapira_Grpc_Call_Context, __construct, arginfo_class_Rapira_Grpc_Call_Context___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Responder_ResponseMetadata_methods[] = {
	ZEND_RAW_FENTRY("addHeader", NULL, arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addHeader, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("addBinaryHeader", NULL, arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryHeader, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("addTrailer", NULL, arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addTrailer, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("addBinaryTrailer", NULL, arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_addBinaryTrailer, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("headers", NULL, arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_headers, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_RAW_FENTRY("trailers", NULL, arginfo_class_Rapira_Grpc_Responder_ResponseMetadata_trailers, ZEND_ACC_PUBLIC|ZEND_ACC_ABSTRACT, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Grpc_Dispatcher_methods[] = {
	ZEND_ME(Rapira_Internal_Grpc_Dispatcher, __construct, arginfo_class_Rapira_Internal_Grpc_Dispatcher___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Grpc_Dispatcher, name, arginfo_class_Rapira_Internal_Grpc_Dispatcher_name, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_Dispatcher, tryReceive, arginfo_class_Rapira_Internal_Grpc_Dispatcher_tryReceive, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_Dispatcher, receive, arginfo_class_Rapira_Internal_Grpc_Dispatcher_receive, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_Dispatcher, getInfo, arginfo_class_Rapira_Internal_Grpc_Dispatcher_getInfo, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_Dispatcher, getServices, arginfo_class_Rapira_Internal_Grpc_Dispatcher_getServices, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Grpc_DispatcherInfo_methods[] = {
	ZEND_ME(Rapira_Internal_Grpc_DispatcherInfo, __construct, arginfo_class_Rapira_Internal_Grpc_DispatcherInfo___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Grpc_DispatcherInfo, pendingCount, arginfo_class_Rapira_Internal_Grpc_DispatcherInfo_pendingCount, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_DispatcherInfo, activeCount, arginfo_class_Rapira_Internal_Grpc_DispatcherInfo_activeCount, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Grpc_UnaryCall_methods[] = {
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, __construct, arginfo_class_Rapira_Internal_Grpc_UnaryCall___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, isFinalized, arginfo_class_Rapira_Internal_Grpc_UnaryCall_isFinalized, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, isCancelled, arginfo_class_Rapira_Internal_Grpc_UnaryCall_isCancelled, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, __destruct, arginfo_class_Rapira_Internal_Grpc_UnaryCall___destruct, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, getContext, arginfo_class_Rapira_Internal_Grpc_UnaryCall_getContext, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, getMessage, arginfo_class_Rapira_Internal_Grpc_UnaryCall_getMessage, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, getResponseMetadata, arginfo_class_Rapira_Internal_Grpc_UnaryCall_getResponseMetadata, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, respond, arginfo_class_Rapira_Internal_Grpc_UnaryCall_respond, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_UnaryCall, fail, arginfo_class_Rapira_Internal_Grpc_UnaryCall_fail, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Internal_Grpc_ResponseMetadata_methods[] = {
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, __construct, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata___construct, ZEND_ACC_PRIVATE)
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, addHeader, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addHeader, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, addBinaryHeader, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addBinaryHeader, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, addTrailer, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addTrailer, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, addBinaryTrailer, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_addBinaryTrailer, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, headers, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_headers, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Internal_Grpc_ResponseMetadata, trailers, arginfo_class_Rapira_Internal_Grpc_ResponseMetadata_trailers, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Grpc_Exception_GrpcException_methods[] = {
	ZEND_ME(Rapira_Grpc_Exception_GrpcException, __construct, arginfo_class_Rapira_Grpc_Exception_GrpcException___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static zend_class_entry *register_class_Rapira_Grpc_GrpcDispatcher(zend_class_entry *class_entry_Rapira_Dispatcher)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "GrpcDispatcher", class_Rapira_Grpc_GrpcDispatcher_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Dispatcher);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_GrpcDispatcherInfo(zend_class_entry *class_entry_Rapira_DispatcherInfo)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "GrpcDispatcherInfo", NULL);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_DispatcherInfo);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Call(zend_class_entry *class_entry_Rapira_Work)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "Call", class_Rapira_Grpc_Call_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Work);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Responder(zend_class_entry *class_entry_Rapira_Work)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "Responder", class_Rapira_Grpc_Responder_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Work);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_UnaryRequest(zend_class_entry *class_entry_Rapira_Grpc_Call)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "UnaryRequest", class_Rapira_Grpc_UnaryRequest_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Grpc_Call);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_UnaryResponder(zend_class_entry *class_entry_Rapira_Grpc_Responder)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "UnaryResponder", class_Rapira_Grpc_UnaryResponder_methods);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Grpc_Responder);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_UnaryCall(zend_class_entry *class_entry_Rapira_Grpc_UnaryRequest, zend_class_entry *class_entry_Rapira_Grpc_UnaryResponder)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "UnaryCall", NULL);
	class_entry = zend_register_internal_interface(&ce);
	zend_class_implements(class_entry, 2, class_entry_Rapira_Grpc_UnaryRequest, class_entry_Rapira_Grpc_UnaryResponder);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Metadata(zend_class_entry *class_entry_Countable, zend_class_entry *class_entry_IteratorAggregate)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "Metadata", class_Rapira_Grpc_Metadata_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);
	zend_class_implements(class_entry, 2, class_entry_Countable, class_entry_IteratorAggregate);

	zval property_entries_default_value;
	ZVAL_UNDEF(&property_entries_default_value);
	zend_string *property_entries_name = zend_string_init("entries", sizeof("entries") - 1, 1);
	zend_declare_typed_property(class_entry, property_entries_name, &property_entries_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_entries_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_StatusCode(void)
{
	zend_class_entry *class_entry = zend_register_internal_enum("Rapira\\Grpc\\StatusCode", IS_LONG, NULL);

	zval enum_case_Cancelled_value;
	ZVAL_LONG(&enum_case_Cancelled_value, 1);
	zend_enum_add_case_cstr(class_entry, "Cancelled", &enum_case_Cancelled_value);

	zval enum_case_Unknown_value;
	ZVAL_LONG(&enum_case_Unknown_value, 2);
	zend_enum_add_case_cstr(class_entry, "Unknown", &enum_case_Unknown_value);

	zval enum_case_InvalidArgument_value;
	ZVAL_LONG(&enum_case_InvalidArgument_value, 3);
	zend_enum_add_case_cstr(class_entry, "InvalidArgument", &enum_case_InvalidArgument_value);

	zval enum_case_DeadlineExceeded_value;
	ZVAL_LONG(&enum_case_DeadlineExceeded_value, 4);
	zend_enum_add_case_cstr(class_entry, "DeadlineExceeded", &enum_case_DeadlineExceeded_value);

	zval enum_case_NotFound_value;
	ZVAL_LONG(&enum_case_NotFound_value, 5);
	zend_enum_add_case_cstr(class_entry, "NotFound", &enum_case_NotFound_value);

	zval enum_case_AlreadyExists_value;
	ZVAL_LONG(&enum_case_AlreadyExists_value, 6);
	zend_enum_add_case_cstr(class_entry, "AlreadyExists", &enum_case_AlreadyExists_value);

	zval enum_case_PermissionDenied_value;
	ZVAL_LONG(&enum_case_PermissionDenied_value, 7);
	zend_enum_add_case_cstr(class_entry, "PermissionDenied", &enum_case_PermissionDenied_value);

	zval enum_case_ResourceExhausted_value;
	ZVAL_LONG(&enum_case_ResourceExhausted_value, 8);
	zend_enum_add_case_cstr(class_entry, "ResourceExhausted", &enum_case_ResourceExhausted_value);

	zval enum_case_FailedPrecondition_value;
	ZVAL_LONG(&enum_case_FailedPrecondition_value, 9);
	zend_enum_add_case_cstr(class_entry, "FailedPrecondition", &enum_case_FailedPrecondition_value);

	zval enum_case_Aborted_value;
	ZVAL_LONG(&enum_case_Aborted_value, 10);
	zend_enum_add_case_cstr(class_entry, "Aborted", &enum_case_Aborted_value);

	zval enum_case_OutOfRange_value;
	ZVAL_LONG(&enum_case_OutOfRange_value, 11);
	zend_enum_add_case_cstr(class_entry, "OutOfRange", &enum_case_OutOfRange_value);

	zval enum_case_Unimplemented_value;
	ZVAL_LONG(&enum_case_Unimplemented_value, 12);
	zend_enum_add_case_cstr(class_entry, "Unimplemented", &enum_case_Unimplemented_value);

	zval enum_case_Internal_value;
	ZVAL_LONG(&enum_case_Internal_value, 13);
	zend_enum_add_case_cstr(class_entry, "Internal", &enum_case_Internal_value);

	zval enum_case_Unavailable_value;
	ZVAL_LONG(&enum_case_Unavailable_value, 14);
	zend_enum_add_case_cstr(class_entry, "Unavailable", &enum_case_Unavailable_value);

	zval enum_case_DataLoss_value;
	ZVAL_LONG(&enum_case_DataLoss_value, 15);
	zend_enum_add_case_cstr(class_entry, "DataLoss", &enum_case_DataLoss_value);

	zval enum_case_Unauthenticated_value;
	ZVAL_LONG(&enum_case_Unauthenticated_value, 16);
	zend_enum_add_case_cstr(class_entry, "Unauthenticated", &enum_case_Unauthenticated_value);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_MethodKind(void)
{
	zend_class_entry *class_entry = zend_register_internal_enum("Rapira\\Grpc\\MethodKind", IS_STRING, class_Rapira_Grpc_MethodKind_methods);

	zval enum_case_Unary_value;
	zend_string *enum_case_Unary_value_str = zend_string_init("unary", strlen("unary"), 1);
	ZVAL_STR(&enum_case_Unary_value, enum_case_Unary_value_str);
	zend_enum_add_case_cstr(class_entry, "Unary", &enum_case_Unary_value);

	zval enum_case_ServerStreaming_value;
	zend_string *enum_case_ServerStreaming_value_str = zend_string_init("server-streaming", strlen("server-streaming"), 1);
	ZVAL_STR(&enum_case_ServerStreaming_value, enum_case_ServerStreaming_value_str);
	zend_enum_add_case_cstr(class_entry, "ServerStreaming", &enum_case_ServerStreaming_value);

	zval enum_case_ClientStreaming_value;
	zend_string *enum_case_ClientStreaming_value_str = zend_string_init("client-streaming", strlen("client-streaming"), 1);
	ZVAL_STR(&enum_case_ClientStreaming_value, enum_case_ClientStreaming_value_str);
	zend_enum_add_case_cstr(class_entry, "ClientStreaming", &enum_case_ClientStreaming_value);

	zval enum_case_BidiStreaming_value;
	zend_string *enum_case_BidiStreaming_value_str = zend_string_init("bidi-streaming", strlen("bidi-streaming"), 1);
	ZVAL_STR(&enum_case_BidiStreaming_value, enum_case_BidiStreaming_value_str);
	zend_enum_add_case_cstr(class_entry, "BidiStreaming", &enum_case_BidiStreaming_value);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_ErrorDetail(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "ErrorDetail", class_Rapira_Grpc_ErrorDetail_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_typeUrl_default_value;
	ZVAL_UNDEF(&property_typeUrl_default_value);
	zend_string *property_typeUrl_name = zend_string_init("typeUrl", sizeof("typeUrl") - 1, 1);
	zend_declare_typed_property(class_entry, property_typeUrl_name, &property_typeUrl_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_typeUrl_name);

	zval property_value_default_value;
	ZVAL_UNDEF(&property_value_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_VALUE), &property_value_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Status(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "Status", class_Rapira_Grpc_Status_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_code_default_value;
	ZVAL_UNDEF(&property_code_default_value);
	zend_string *property_code_class_Rapira_Grpc_StatusCode = zend_string_init("Rapira\\Grpc\\StatusCode", sizeof("Rapira\\Grpc\\StatusCode")-1, 1);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_CODE), &property_code_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_code_class_Rapira_Grpc_StatusCode, 0, 0));

	zval property_message_default_value;
	ZVAL_UNDEF(&property_message_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_MESSAGE), &property_message_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_details_default_value;
	ZVAL_UNDEF(&property_details_default_value);
	zend_string *property_details_name = zend_string_init("details", sizeof("details") - 1, 1);
	zend_declare_typed_property(class_entry, property_details_name, &property_details_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_details_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_MethodInfo(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "MethodInfo", class_Rapira_Grpc_MethodInfo_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_name_default_value;
	ZVAL_UNDEF(&property_name_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_NAME), &property_name_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_inputType_default_value;
	ZVAL_UNDEF(&property_inputType_default_value);
	zend_string *property_inputType_name = zend_string_init("inputType", sizeof("inputType") - 1, 1);
	zend_declare_typed_property(class_entry, property_inputType_name, &property_inputType_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_inputType_name);

	zval property_outputType_default_value;
	ZVAL_UNDEF(&property_outputType_default_value);
	zend_string *property_outputType_name = zend_string_init("outputType", sizeof("outputType") - 1, 1);
	zend_declare_typed_property(class_entry, property_outputType_name, &property_outputType_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_outputType_name);

	zval property_kind_default_value;
	ZVAL_UNDEF(&property_kind_default_value);
	zend_string *property_kind_name = zend_string_init("kind", sizeof("kind") - 1, 1);
	zend_string *property_kind_class_Rapira_Grpc_MethodKind = zend_string_init("Rapira\\Grpc\\MethodKind", sizeof("Rapira\\Grpc\\MethodKind")-1, 1);
	zend_declare_typed_property(class_entry, property_kind_name, &property_kind_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_kind_class_Rapira_Grpc_MethodKind, 0, 0));
	zend_string_release(property_kind_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_ServiceInfo(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc", "ServiceInfo", class_Rapira_Grpc_ServiceInfo_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_name_default_value;
	ZVAL_UNDEF(&property_name_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_NAME), &property_name_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_methods_default_value;
	ZVAL_UNDEF(&property_methods_default_value);
	zend_string *property_methods_name = zend_string_init("methods", sizeof("methods") - 1, 1);
	zend_declare_typed_property(class_entry, property_methods_name, &property_methods_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_ARRAY));
	zend_string_release(property_methods_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Call_Protocol(void)
{
	zend_class_entry *class_entry = zend_register_internal_enum("Rapira\\Grpc\\Call\\Protocol", IS_STRING, NULL);

	zval enum_case_Grpc_value;
	zend_string *enum_case_Grpc_value_str = zend_string_init("grpc", strlen("grpc"), 1);
	ZVAL_STR(&enum_case_Grpc_value, enum_case_Grpc_value_str);
	zend_enum_add_case_cstr(class_entry, "Grpc", &enum_case_Grpc_value);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Call_Context(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc\\Call", "Context", class_Rapira_Grpc_Call_Context_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE|ZEND_ACC_READONLY_CLASS);

	zval property_method_default_value;
	ZVAL_UNDEF(&property_method_default_value);
	zend_string *property_method_name = zend_string_init("method", sizeof("method") - 1, 1);
	zend_declare_typed_property(class_entry, property_method_name, &property_method_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_method_name);

	zval property_metadata_default_value;
	ZVAL_UNDEF(&property_metadata_default_value);
	zend_string *property_metadata_name = zend_string_init("metadata", sizeof("metadata") - 1, 1);
	zend_string *property_metadata_class_Rapira_Grpc_Metadata = zend_string_init("Rapira\\Grpc\\Metadata", sizeof("Rapira\\Grpc\\Metadata")-1, 1);
	zend_declare_typed_property(class_entry, property_metadata_name, &property_metadata_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_metadata_class_Rapira_Grpc_Metadata, 0, 0));
	zend_string_release(property_metadata_name);

	zval property_deadline_default_value;
	ZVAL_UNDEF(&property_deadline_default_value);
	zend_string *property_deadline_name = zend_string_init("deadline", sizeof("deadline") - 1, 1);
	zend_declare_typed_property(class_entry, property_deadline_name, &property_deadline_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_DOUBLE|MAY_BE_NULL));
	zend_string_release(property_deadline_name);

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

	zval property_tls_default_value;
	ZVAL_UNDEF(&property_tls_default_value);
	zend_string *property_tls_name = zend_string_init("tls", sizeof("tls") - 1, 1);
	zend_string *property_tls_class_Rapira_Tls = zend_string_init("Rapira\\Tls", sizeof("Rapira\\Tls")-1, 1);
	zend_declare_typed_property(class_entry, property_tls_name, &property_tls_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_tls_class_Rapira_Tls, 0, MAY_BE_NULL));
	zend_string_release(property_tls_name);

	zval property_protocol_default_value;
	ZVAL_UNDEF(&property_protocol_default_value);
	zend_string *property_protocol_name = zend_string_init("protocol", sizeof("protocol") - 1, 1);
	zend_string *property_protocol_class_Rapira_Grpc_Call_Protocol = zend_string_init("Rapira\\Grpc\\Call\\Protocol", sizeof("Rapira\\Grpc\\Call\\Protocol")-1, 1);
	zend_declare_typed_property(class_entry, property_protocol_name, &property_protocol_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_protocol_class_Rapira_Grpc_Call_Protocol, 0, 0));
	zend_string_release(property_protocol_name);

	zval property_receivedAt_default_value;
	ZVAL_UNDEF(&property_receivedAt_default_value);
	zend_string *property_receivedAt_name = zend_string_init("receivedAt", sizeof("receivedAt") - 1, 1);
	zend_declare_typed_property(class_entry, property_receivedAt_name, &property_receivedAt_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_DOUBLE));
	zend_string_release(property_receivedAt_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Responder_ResponseMetadata(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc\\Responder", "ResponseMetadata", class_Rapira_Grpc_Responder_ResponseMetadata_methods);
	class_entry = zend_register_internal_interface(&ce);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Grpc_Dispatcher(zend_class_entry *class_entry_Rapira_Grpc_GrpcDispatcher)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Grpc", "Dispatcher", class_Rapira_Internal_Grpc_Dispatcher_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Grpc_GrpcDispatcher);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Grpc_DispatcherInfo(zend_class_entry *class_entry_Rapira_Grpc_GrpcDispatcherInfo)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Grpc", "DispatcherInfo", class_Rapira_Internal_Grpc_DispatcherInfo_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Grpc_GrpcDispatcherInfo);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Grpc_UnaryCall(zend_class_entry *class_entry_Rapira_Grpc_UnaryCall)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Grpc", "UnaryCall", class_Rapira_Internal_Grpc_UnaryCall_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Grpc_UnaryCall);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Internal_Grpc_ResponseMetadata(zend_class_entry *class_entry_Rapira_Grpc_Responder_ResponseMetadata)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Internal\\Grpc", "ResponseMetadata", class_Rapira_Internal_Grpc_ResponseMetadata_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_NO_DYNAMIC_PROPERTIES|ZEND_ACC_NOT_SERIALIZABLE);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Grpc_Responder_ResponseMetadata);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Grpc_Exception_GrpcException(zend_class_entry *class_entry_RuntimeException, zend_class_entry *class_entry_Rapira_Exception_RapiraThrowable)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Grpc\\Exception", "GrpcException", class_Rapira_Grpc_Exception_GrpcException_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_RuntimeException, 0);
	zend_class_implements(class_entry, 1, class_entry_Rapira_Exception_RapiraThrowable);

	zval property_status_default_value;
	ZVAL_UNDEF(&property_status_default_value);
	zend_string *property_status_name = zend_string_init("status", sizeof("status") - 1, 1);
	zend_string *property_status_class_Rapira_Grpc_Status = zend_string_init("Rapira\\Grpc\\Status", sizeof("Rapira\\Grpc\\Status")-1, 1);
	zend_declare_typed_property(class_entry, property_status_name, &property_status_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_status_class_Rapira_Grpc_Status, 0, 0));
	zend_string_release(property_status_name);

	return class_entry;
}
