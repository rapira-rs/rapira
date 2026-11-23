use std::ffi::{CStr, c_int};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{Builder, JoinHandle};

use anyhow::{Context, ensure};
use crossbeam_channel::Sender;
use rapira_config::PoolSettings;

use crate::*;

unsafe extern "C" {
    fn rapira_process_init(expected_threads: c_int, disable_timeouts: bool) -> bool;
    fn rapira_process_shutdown();
}

/// True when this PHP build supports one interpreter per thread.
pub fn zts() -> bool {
    // SAFETY: the shim returns a build constant.
    unsafe { rapira_zts() }
}

/// True when PHP can arm a separate execution timer for each worker.
pub fn has_execution_timers() -> bool {
    // SAFETY: the shim returns a build constant.
    unsafe { rapira_has_execution_timers() }
}

/// The version string of the linked libphp.
pub fn linked_php_version() -> String {
    // SAFETY: php_version returns a static C string before and after startup.
    unsafe { CStr::from_ptr(php_version()) }
        .to_string_lossy()
        .into_owned()
}

/// The layouts that bindgen read must match the PHP minor of the linked library.
fn check_linked_php() -> anyhow::Result<()> {
    // SAFETY: php_version_id returns a constant and needs no interpreter.
    let linked = unsafe { php_version_id() };
    ensure!(
        linked / 100 == PHP_VERSION_ID / 100,
        "linked libphp is PHP {} (PHP_VERSION_ID {linked}), but this rapira was built against PHP_VERSION_ID {PHP_VERSION_ID}. \
         Use a libphp from the same PHP minor as the build.",
        linked_php_version()
    );
    Ok(())
}

#[derive(Default)]
pub(crate) struct Workers(Mutex<Vec<(Sender<()>, JoinHandle<()>)>>);

impl Workers {
    pub(crate) fn spawn(
        &self,
        builder: Builder,
        control: Sender<()>,
        run: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let mut workers = self.0.lock().unwrap();
        let thread = builder.spawn(run)?;
        workers.push((control, thread));
        Ok(())
    }

    pub(crate) fn wake(&self) {
        for (control, _) in self.0.lock().unwrap().iter() {
            // A full channel already holds a reload for this worker.
            let _ = control.try_send(());
        }
    }
}

/// The main thread owns PHP startup, the worker roster and PHP shutdown.
pub struct Engine {
    pub(crate) workers: Arc<Workers>,
    pub(crate) pools: Vec<Arc<pool::Pool>>,
    /// Runs from the start of the first pool with a request time limit until the drain starts.
    pub(crate) watchdog: Option<crate::watchdog::Watchdog>,
    _main_thread: PhantomData<Rc<()>>,
}

impl Engine {
    /// Starts TSRM and PHP for the workers of `pools`. MINIT calls the plugin register functions in order.
    pub fn start<'a>(
        register: &'static [unsafe extern "C" fn()],
        pools: impl IntoIterator<Item = &'a PoolSettings>,
    ) -> anyhow::Result<Self> {
        static STARTED: AtomicBool = AtomicBool::new(false);
        check_linked_php()?;
        let mut workers = 0usize;
        let mut timed = false;
        for pool in pools {
            workers = workers
                .checked_add(pool.workers)
                .context("too many PHP workers")?;
            timed |= !pool.request_terminate_timeout.is_zero();
        }
        let expected_threads = c_int::try_from(workers.saturating_add(1))
            .context("too many PHP worker threads for TSRM")?;
        ensure!(
            !STARTED.swap(true, Ordering::Relaxed),
            "the PHP engine can start only once per process"
        );

        let disable_timeouts = workers > 1 && !has_execution_timers();
        let mut module = module::build_sapi_module();
        // SAFETY: only this thread starts PHP. The register functions stay valid for the life of the process.
        let started = unsafe {
            rapira_plugin_registers = register.as_ptr().cast();
            rapira_plugin_register_count = register.len();
            ensure!(
                rapira_process_init(expected_threads, disable_timeouts),
                "PHP thread startup failed"
            );
            sapi_startup(&mut module);
            if disable_timeouts {
                // sapi_startup() clears ini_entries. https://github.com/php/php-src/blob/php-8.6.0/main/SAPI.c#L66-L69
                // PHP parses these entries last. https://github.com/php/php-src/blob/php-8.6.0/main/php_ini.c#L716-L727
                module.ini_entries = c"max_execution_time=0\nmax_input_time=-1\n".as_ptr();
                tracing::warn!(target: "rapira", "PHP has no per-thread execution timers; using max_execution_time=0 and max_input_time=-1 for {workers} workers");
            }
            php_module_startup(&mut module, &raw mut rapira_module_entry) == SUCCESS
        };
        let engine = Self {
            workers: Arc::default(),
            pools: Vec::new(),
            watchdog: None,
            _main_thread: PhantomData,
        };
        ensure!(started, "php_module_startup failed");
        // OPcache preloading starts a PHP request. https://github.com/php/php-src/blob/php-8.6.0/ext/opcache/ZendAccelerator.c#L5152-L5173
        // Request startup sets the timeout with reset_signals enabled. https://github.com/php/php-src/blob/php-8.6.0/main/main.c#L1886-L1890
        // This can unblock the timer signal on the main thread. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_execute_API.c#L1656-L1672
        crate::watchdog::block_timer_signal()?;
        crate::reload::check_opcache(workers)?;
        // SAFETY: MINIT is complete and no worker can read the hooks yet.
        unsafe {
            rapira_register_post_reader();
            zend_accel_schedule_restart_hook =
                Some(crate::rapira_worker::opcache_restart_scheduled);
        }
        crate::env::install();
        crate::context::intern_server_strings();
        if timed {
            // The reservation blocks the wake signal on this thread, and each later thread inherits the mask.
            crate::watchdog::reserve_wake_signal()
                .context("the request watchdog requires a free SIGRTMAX")?;
        }
        Ok(engine)
    }

    pub fn workers_finished(&self) -> bool {
        self.workers
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(_, t)| t.is_finished())
    }

    pub fn reload(&self) {
        crate::reload::request_reset();
        self.workers.wake();
    }

    /// The record of each pool, in start order.
    pub fn pools(&self) -> &[Arc<pool::Pool>] {
        &self.pools
    }

    /// Starts the drain and stops the watchdog. A worker with a unit in progress stays active until its next pull.
    pub fn begin_drain(&mut self) {
        for pool in &self.pools {
            pool.begin_drain();
        }
        self.watchdog = None;
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let workers = std::mem::take(&mut *self.workers.0.lock().unwrap());
        for (control, worker) in workers {
            drop(control);
            worker.join().expect("the worker catches its panics");
        }
        // SAFETY: this is the thread that started TSRM. Every PHP worker has joined.
        unsafe { rapira_process_shutdown() };
    }
}
