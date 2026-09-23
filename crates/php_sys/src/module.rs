use crate::*;
use callbacks;
use std::{
    os::raw::c_char,
    ptr::{null, null_mut},
};

/// Runs before php.ini is parsed (main/php_ini.c:420) so an explicit user value still wins; startup-stage interned strings are permanent, so config_zval_dtor's release is a no-op.
unsafe extern "C" fn ini_defaults(configuration_hash: *mut HashTable) {
    for (name, value) in [(c"display_errors", c"0"), (c"log_errors", c"1")] {
        unsafe {
            let intern = zend_string_init_interned.expect("set by zend_startup");
            let mut v: zval = std::mem::zeroed();
            v.value.str_ = intern(value.as_ptr(), value.count_bytes(), true);
            v.u1.type_info = IS_STRING;
            zend_hash_str_update(
                configuration_hash,
                name.as_ptr(),
                name.count_bytes(),
                &mut v,
            );
        }
    }
}

/// On 8.4 the SAPI name must be "fastcgi": OPcache <= 8.4 starts only for names on accel_find_sapi()'s allowlist, removed in 8.5 (https://github.com/php/php-src/commit/3088d6406847dd425dd43122f5de57cc97aa4408).
pub(crate) fn build_sapi_module() -> sapi_module_struct {
    sapi_module_struct {
        #[cfg(php84)]
        name: c"fastcgi".as_ptr() as *mut c_char,
        #[cfg(not(php84))]
        name: c"rapira".as_ptr() as *mut c_char,
        pretty_name: c"Rapira".as_ptr() as *mut c_char,
        startup: None,
        shutdown: None,
        activate: None,
        deactivate: None,
        ub_write: Some(rapira_ub_write),
        flush: None,
        get_stat: None,
        getenv: Some(callbacks::getenv_cb),
        header_handler: None,
        send_headers: Some(callbacks::send_headers),
        send_header: None,
        read_post: Some(callbacks::read_post),
        read_cookies: Some(callbacks::read_cookies),
        register_server_variables: Some(callbacks::register_server_variables),
        log_message: Some(callbacks::log_message),
        get_request_time: None,
        terminate_process: None,
        php_ini_path_override: null_mut(),
        default_post_reader: Some(php_default_post_reader),
        treat_data: Some(php_default_treat_data),
        executable_location: null_mut(),
        php_ini_ignore: 0,
        php_ini_ignore_cwd: 1,
        get_fd: None,
        force_http_10: None,
        get_target_gid: None,
        get_target_uid: None,
        input_filter: Some(php_default_input_filter),
        ini_defaults: Some(ini_defaults),
        phpinfo_as_text: 0,
        ini_entries: null_mut(),
        additional_functions: null(),
        input_filter_init: None,
        sapi_error: Some(zend_error),
        #[cfg(php85)]
        pre_request_init: None,
    }
}
