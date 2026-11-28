use std::ffi::{CStr, c_int};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{Builder, JoinHandle};

use anyhow::{Context, ensure};
use rapira_config::PoolSettings;

use crate::queue::PoolQueue;
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
struct WorkersInner {
    threads: Vec<JoinHandle<()>>,
    queues: Vec<Arc<PoolQueue>>,
    active: usize,
    on_all_done: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
pub(crate) struct Workers(Mutex<WorkersInner>);

struct ActiveGuard(Arc<Workers>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let notify = {
            let mut inner = self.0.0.lock().unwrap();
            inner.active -= 1;
            if inner.active == 0 {
                inner.on_all_done.clone()
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            notify();
        }
    }
}

impl Workers {
    pub(crate) fn register_queue(&self, queue: Arc<PoolQueue>) {
        self.0.lock().unwrap().queues.push(queue);
    }

    pub(crate) fn spawn(
        self: &Arc<Self>,
        builder: Builder,
        run: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let mut inner = self.0.lock().unwrap();
        inner.active += 1;
        let workers = Arc::clone(self);
        match builder.spawn(move || {
            let _active = ActiveGuard(workers);
            run();
        }) {
            Ok(thread) => {
                inner.threads.push(thread);
                Ok(())
            }
            Err(e) => {
                inner.active -= 1;
                Err(e)
            }
        }
    }

    pub(crate) fn wake(&self) {
        let queues = self.0.lock().unwrap().queues.clone();
        for queue in queues {
            queue.reload();
        }
    }
}

/// The main thread owns PHP startup, the list of worker threads and PHP shutdown.
pub struct Engine {
    pub(crate) workers: Arc<Workers>,
    pub(crate) pools: Vec<Arc<pool::Pool>>,
    pub(crate) opcache: Arc<crate::reload::OpcacheCoordinator>,
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

        let disable_timeouts = workers > 1 && !unsafe { rapira_has_execution_timers() };
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
            opcache: Arc::default(),
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
            zend_accel_schedule_restart_hook = Some(crate::worker::opcache_restart_scheduled);
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

    /// Registers a callback invoked once all spawned PHP worker threads finish (or immediately if none are active).
    pub fn set_workers_done_notify(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        let already_done = {
            let mut inner = self.workers.0.lock().unwrap();
            inner.on_all_done = Some(Arc::clone(&notify));
            inner.active == 0
        };
        if already_done {
            notify();
        }
    }

    pub fn reload(&self) {
        self.opcache.request_reset();
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
        let (queues, threads) = {
            let mut inner = self.workers.0.lock().unwrap();
            (
                std::mem::take(&mut inner.queues),
                std::mem::take(&mut inner.threads),
            )
        };
        for queue in queues {
            queue.close();
        }
        for worker in threads {
            worker.join().expect("the worker catches its panics");
        }
        // SAFETY: this is the thread that started TSRM. Every PHP worker has joined.
        unsafe { rapira_process_shutdown() };
    }
}
