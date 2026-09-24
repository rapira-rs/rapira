bind! {
    sapi_module_struct, sapi_headers_struct, sapi_header_struct, sapi_request_info,
    sapi_globals_struct, zend_executor_globals, php_core_globals, zend_compiler_globals,
    zend_file_handle, zend_module_entry, zend_string, zval, HashTable, zend_long,
    zend_fcall_info, zend_fcall_info_cache,
    sapi_startup, sapi_shutdown, php_module_startup, php_module_shutdown, php_request_startup,
    php_execute_script, zend_error, zend_stream_init_filename, zend_destroy_file_handle,
    php_register_variable_safe, rapira_mode, RAPIRA_MODE_CLASSIC, RAPIRA_MODE_WORKER,
    RAPIRA_MODE_DISPATCHER,
    // the two halves of the linked-libphp version check
    PHP_VERSION_ID, php_version_id,
    // the embedded-object layouts; wrapper.h is the source of truth
    rapira_exchange_obj, rapira_dispatcher_info_obj, rapira_grpc_call_obj,
    rapira_grpc_metadata_obj,
    // MINIT-written class-entry globals the Rust builder reads (static mut)
    rapira_ce_http_request, rapira_ce_http_multipart, rapira_ce_http_form_field,
    rapira_ce_http_uploaded_file, rapira_ce_tls, rapira_ce_inet_address,
    rapira_ce_unix_address, rapira_ce_already_finalized_error,
    rapira_ce_http_head_already_written_error, rapira_ce_internal_http_exchange,
    rapira_ce_internal_http_dispatcher, rapira_ce_internal_http_dispatcher_info,
    rapira_ce_timeout_exception, rapira_ce_closed_exception,
    rapira_ce_no_dispatcher_error, rapira_ce_grpc_status, rapira_ce_grpc_exception,
    rapira_ce_grpc_method_kind, rapira_ce_grpc_method_info, rapira_ce_grpc_service_info,
    rapira_ce_internal_grpc_dispatcher, rapira_ce_internal_grpc_dispatcher_info,
    rapira_ce_internal_grpc_unary_call, rapira_ce_internal_grpc_response_metadata,
    rapira_ce_grpc_context, rapira_ce_grpc_metadata, rapira_ce_grpc_protocol,
    rapira_ce_grpc_error_detail,
    zend_argument_value_error, zend_argument_type_error,
    rapira_ce_work_discarded_exception, rapira_ce_http_content_length_exceeded_error,
    rapira_ce_http_head_not_written_error, rapira_ce_http_file_not_sendable_exception,
    // re-arms the wall timer the send-park guard disarms around a full-channel wait
    zend_set_timeout,
    // the FOREACH macros are header-only, so the array walk uses the exported position API
    zend_hash_internal_pointer_reset_ex, zend_hash_get_current_key_ex,
    zend_hash_get_current_data_ex, zend_hash_move_forward_ex, HashPosition,
    IS_NULL, IS_ARRAY, IS_REFERENCE,
    // zend_throw_error/zend_value_error are varargs, called with a fixed format
    zend_throw_error, zend_value_error, zend_throw_exception,
    // instanceof_function is inline; only its slow path is exported
    zend_update_property_str, instanceof_function_slow, zend_zval_value_name, IS_OBJECT,
    zend_read_property, zend_get_exception_base, zend_ce_throwable, php_json_encode,
    smart_str, PHP_JSON_PARTIAL_OUTPUT_ON_ERROR,
    // add_assoc_str/add_index_zval/smart_str_free are inline: these are their exported carriers and shim
    add_assoc_stringl_ex, zend_hash_index_update, rapira_smart_str_free,
    // zend_update_property*: the EG(fake_scope) swap inside is what initializes readonly props
    object_init_ex, zend_update_property, zend_update_property_stringl,
    zend_update_property_long, zend_update_property_double, zend_update_property_null,
    // array_init_size is a macro -> rapira_array_init shim in wrapper.c
    rapira_array_init,
    // zend_symtable_str_find is inline -> rapira_symtable_str_find shim in wrapper.c
    rapira_symtable_str_find,
    // ZVAL_OBJ_COPY is a macro -> rapira_zval_enum_case shim in wrapper.c
    rapira_zval_enum_case,
    // zend_symtable_str_update is inline; add_assoc_zval_ex is its exported caller (zend_API.c)
    add_assoc_zval_ex, add_next_index_stringl, add_next_index_object,
    zval_add_ref,
    zend_object, zend_class_entry, zval_ptr_dtor, // zval_ptr_dtor_nogc is inline-only -> use zval_ptr_dtor
    // zend_string_init is inline-only; the exported interner fn-pointer covers startup-time strings
    zend_hash_str_update, zend_string_init_interned,
    php_default_post_reader, php_default_treat_data, php_default_input_filter,
    zend_unset_timeout, php_handle_auth_data,
    SAPI_HEADER_SENT_SUCCESSFULLY, SAPI_HEADER_SEND_FAILED, IS_UNDEF, IS_STRING,
    E_WARNING, E_CORE_WARNING, E_COMPILE_WARNING, E_USER_WARNING,
    E_NOTICE, E_USER_NOTICE, E_DEPRECATED, E_USER_DEPRECATED,
    E_CORE, E_FATAL_ERRORS, // php-src's own groupings
}
