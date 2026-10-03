pub mod bindings;

pub mod callbacks;
pub mod classic_worker;
pub mod context;
pub mod diagnostics;
pub mod dispatcher;
pub mod exchange;
pub mod module;
pub mod plugin;
pub mod quota;
pub mod rapira_worker;
pub mod scoreboard;
pub mod start;
pub mod types;
pub mod values;
pub mod work;
pub mod zend;

use std::ffi::c_int;

pub use bindings::*;
pub use quota::WorkerHooks;
pub use start::{PhpModule, Rapira, boot_master, linked_php_version};
pub use types::{Addr, Frame, Request};

// bindgen names the Zend SUCCESS/FAILURE constants differently across php-src versions, so the values are hardcoded.
pub const SUCCESS: c_int = 0;
pub const FAILURE: c_int = -1;

// The request shims return RAPIRA_OK, RAPIRA_BAILOUT or RAPIRA_THROW (rapira_sapi.h).
unsafe extern "C" {
    pub fn rapira_finish_output() -> c_int;
    pub fn rapira_init_call_stack();
    pub fn rapira_clear_last_error();
    pub fn rapira_request_teardown() -> c_int;
    pub fn rapira_process_init();
    pub fn rapira_child_init();
    pub fn rapira_release_temporary_streams();
    // Holds boot-registered shutdown functions until cycle end (module.c).
    pub fn rapira_stash_boot_shutdown_functions();
    pub fn rapira_request_activate() -> c_int;
    pub fn rapira_request_shutdown() -> c_int;
    // Wall timer is disarmed while parked in receive() and re-armed with the captured per-cycle budget on unit handout (module.c).
    pub fn rapira_receive_untimed();
    pub fn rapira_receive_timed();
    // C shim (module.c) over rapira_rs_ub_write: raises the client-abort bailout from C so the longjmp does not cross the Rust catch_unwind frame.
    pub fn rapira_ub_write(str_: *const std::os::raw::c_char, len: usize) -> usize;

    pub fn rapira_run_handler(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> c_int;

    pub static mut rapira_module_entry: zend_module_entry;
}
