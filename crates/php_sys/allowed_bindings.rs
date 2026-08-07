bind! {
    // types
    sapi_module_struct, sapi_headers_struct, sapi_header_struct, sapi_request_info,
    sapi_globals_struct, zend_executor_globals, php_core_globals, zend_compiler_globals,
    zend_llist, zend_llist_element, zend_file_handle, zend_module_entry, zend_function_entry,
    zend_execute_data, zend_auto_global, zend_string, zval, HashTable, zend_array, zend_long,
    zend_fcall_info, zend_fcall_info_cache,
    // functions
    sapi_startup, sapi_shutdown, sapi_activate, sapi_deactivate,
    php_module_startup, php_module_shutdown, php_request_startup, php_request_shutdown,
    php_execute_script, zend_error, zend_stream_init_filename, zend_destroy_file_handle,
    php_register_variable_safe, php_output_activate, php_output_deactivate, php_output_end_all,
    zend_activate_auto_globals, rapira_mode, RAPIRA_MODE_CLASSIC, RAPIRA_MODE_WORKER_REQUEST,
    zend_call_function, zend_fcall_info_init, zval_ptr_dtor, zend_hash_str_del,  // zval_ptr_dtor_nogc is inline-only -> use zval_ptr_dtor
    // zend_string_init is inline-only; the exported interner fn-pointer covers startup-time strings
    zend_hash_str_update, zend_string_init_interned,
    php_default_post_reader, php_default_treat_data, php_default_input_filter, zend_call_destructors,
    php_call_shutdown_functions, zend_observer_fcall_end_all, zend_unset_timeout, php_handle_auth_data, php_handle_aborted_connection,
    // consts
    SAPI_HEADER_SENT_SUCCESSFULLY, SAPI_HEADER_SEND_FAILED, SAPI_HEADER_DO_SEND,
    TRACK_VARS_FILES, NUM_TRACK_VARS, IS_UNDEF, IS_TRUE, IS_FALSE, IS_STRING, ZEND_OBSERVER_ENABLED,
    // diagnostic types; E_CORE and E_FATAL_ERRORS are php-src's own groupings
    E_WARNING, E_CORE_WARNING, E_COMPILE_WARNING, E_USER_WARNING,
    E_NOTICE, E_USER_NOTICE, E_DEPRECATED, E_USER_DEPRECATED,
    E_CORE, E_FATAL_ERRORS,
}
