use std::{ffi::CString, path::Path};

use crate::{
    php_execute_script, zend_destroy_file_handle, zend_file_handle, zend_stream_init_filename,
};

/// # Safety
/// Requires an active request. `php_execute_script` catches a bailout itself.
pub unsafe fn run_script(script: &Path) -> bool {
    unsafe {
        let c_script: CString =
            CString::new(script.to_string_lossy().as_bytes()).unwrap_or_default();
        let mut fh: zend_file_handle = std::mem::zeroed();
        zend_stream_init_filename(&mut fh, c_script.as_ptr());
        fh.primary_script = true;
        let ok: bool = php_execute_script(&mut fh);
        zend_destroy_file_handle(&mut fh);
        ok
    }
}
