pub mod bindings;

pub mod callbacks;
mod classic_worker;
mod context;
mod diagnostics;
mod env;
pub mod exchange;
mod log;
mod module;
pub mod plugin;
pub mod quota;
mod rapira_worker;
pub mod scoreboard;
mod start;
pub mod stream;
pub mod types;
pub mod work;
pub mod zend;

use std::ffi::c_int;

pub use bindings::*;
pub use start::{PhpModule, Rapira, StartedWorker, boot_master, linked_php_version};
pub use types::{Addr, Frame, Request};

// bindgen names the Zend SUCCESS and FAILURE constants differently across php-src versions, so the values are hardcoded.
pub const SUCCESS: c_int = 0;
pub const FAILURE: c_int = -1;

// The request shims return RAPIRA_OK, RAPIRA_BAILOUT or RAPIRA_THROW (rapira_sapi.h).
unsafe extern "C" {
    pub fn rapira_finish_output() -> c_int;
    pub fn rapira_clear_last_error();
    pub fn rapira_request_teardown() -> c_int;
    // Starts TSRM on ZTS and the signal handling of Zend, before sapi_startup. rapira_process_shutdown stops the module, SAPI and TSRM. Both run on the main thread (module.c).
    pub fn rapira_process_init(expected_threads: c_int);
    pub fn rapira_process_shutdown();
    pub fn rapira_child_init();
    // Gives the calling thread its PHP globals on ZTS, its call stack limit, the SAPI options and the urlencoded post reader. It runs first on each PHP thread (module.c).
    pub fn rapira_thread_start();
    pub fn rapira_release_temporary_streams();
    // Holds the shutdown functions of the boot until the cycle ends (module.c).
    pub fn rapira_stash_boot_shutdown_functions();
    pub fn rapira_request_activate() -> c_int;
    // Registers the default post reader and the activate hook that give PHP the buffered body. It runs after MINIT (module.c).
    pub fn rapira_register_post_reader();
    pub fn rapira_request_shutdown() -> c_int;
    // rapira_receive_untimed disarms the execution timer while the app waits in receive(). rapira_receive_timed arms it again with the captured per-cycle budget when the host hands out a unit (module.c).
    pub fn rapira_receive_untimed();
    pub fn rapira_receive_timed();
    // Creates the object of a unit in rv and returns the address of its held word, a rapira_held (rapira_dispatcher.c).
    pub fn rapira_unit_object(rv: *mut zval, ce: *mut zend_class_entry) -> *mut std::ffi::c_void;
    // A C shim (module.c) over rapira_rs_ub_write. It raises the client-abort bailout from C, so the longjmp does not cross the Rust catch_unwind frame.
    pub fn rapira_ub_write(str_: *const std::os::raw::c_char, len: usize) -> usize;

    pub fn rapira_run_handler(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> c_int;
    // Returns -1 with no OPcache. Otherwise it returns a set of flags: 1 when OPcache serves this app instance, 2 when a restart is pending, 4 when a restart runs now. Writes last_restart_time (module.c).
    pub fn rapira_opcache_state(last_restart: *mut zend_long) -> c_int;
    // Calls opcache_reset() (module.c).
    pub fn rapira_opcache_reset();
    // Returns a permanent interned string, which PHP never refcounts or frees (wrapper.c).
    pub fn rapira_permanent_string(s: *const std::os::raw::c_char, len: usize) -> *mut zend_string;

    pub static mut rapira_module_entry: zend_module_entry;
    // OPcache calls it in the process that schedules a restart (Zend/zend.h).
    pub static mut zend_accel_schedule_restart_hook: Option<unsafe extern "C" fn(reason: c_int)>;
}
