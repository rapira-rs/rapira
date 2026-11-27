use anyhow::Context as _;
use rapira_config::{ExecutionClock, Mode, PoolSettings};
use std::ffi::{CStr, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;
use tracing::error;

use crate::plugin::Plugin;
use crate::pool::{Event, Pool};
use crate::queue::{Intake, PoolQueue};
use crate::worker::Worker;
use crate::{
    RAPIRA_MODE_CLASSIC, RAPIRA_MODE_DISPATCHER, RAPIRA_MODE_WORKER, php_execute_script,
    rapira_plugin, rapira_thread_end, rapira_thread_start, zend_destroy_file_handle,
    zend_file_handle, zend_stream_init_filename,
};

/// The jitter spreads the recycles across workers.
fn effective_quota(max_requests: u64, index: usize) -> u64 {
    if max_requests == 0 {
        return 0;
    }
    use std::hash::{BuildHasher, RandomState};
    let grace = (max_requests / 2).max(1);
    max_requests.saturating_add(1 + RandomState::new().hash_one(index) % grace)
}

#[derive(Clone, Copy)]
struct ThreadPlugin {
    mode: c_int,
    record: *const rapira_plugin,
    name: &'static str,
    cpu_clock: bool,
}

// SAFETY: the record is immutable from MINIT until all worker threads end.
unsafe impl Send for ThreadPlugin {}

impl ThreadPlugin {
    unsafe fn enter(self) {
        unsafe {
            rapira_thread_start(
                self.mode,
                self.record,
                self.name.as_ptr().cast(),
                self.name.len(),
                self.cpu_clock,
            )
        };
    }
}

impl crate::Engine {
    /// Start the PHP threads and return the submit end of their queue.
    ///
    /// # Safety
    /// The plugin's dispatcher record must stay immutable until engine shutdown.
    pub unsafe fn start_pool(
        &mut self,
        settings: PoolSettings,
        plugin: &dyn Plugin,
        io: &tokio::runtime::Handle,
    ) -> anyhow::Result<Intake> {
        let name = plugin.name();
        let mode = settings.mode;
        let worker_init = plugin.worker_init();
        let plugin = ThreadPlugin {
            name,
            mode: match mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker => RAPIRA_MODE_WORKER,
                Mode::Dispatcher => RAPIRA_MODE_DISPATCHER,
            } as c_int,
            record: plugin.dispatcher(),
            cpu_clock: settings.execution_clock == ExecutionClock::Cpu,
        };
        let (queue, intake) = PoolQueue::new();
        let pool = Arc::new(Pool::new(name, settings.workers, Arc::clone(&queue)));
        self.pools.push(Arc::clone(&pool));
        self.workers.register_queue(Arc::clone(&queue));
        let env = Arc::new(crate::env::PoolEnv::new(&settings.env));
        for index in 0..settings.workers {
            let slot = queue.worker_slot();
            let io = io.clone();
            let init = Arc::clone(&worker_init);
            let pool = Arc::clone(&pool);
            let workers = Arc::clone(&self.workers);
            let opcache = Arc::clone(&self.opcache);
            let env = Arc::clone(&env);
            let entrypoint = settings.entrypoint.clone();
            let max_requests = if mode == Mode::Classic {
                0
            } else {
                effective_quota(settings.max_requests, index)
            };
            let builder = thread::Builder::new()
                .name(format!("rapira-{name}-{index}"))
                .stack_size(8 << 20);
            self.workers
                .spawn(builder, move || {
                    let _entered = io.enter();
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: MINIT has finished. This is the first PHP call on this thread.
                        unsafe { plugin.enter() };
                        let guard = unsafe {
                            crate::watchdog::enter(
                                Arc::clone(&pool),
                                index,
                                settings.request_terminate_timeout,
                            )
                        };
                        pool.bind(index);
                        init();
                        crate::env::enter_pool(env);
                        let worker = Worker::new(
                            slot,
                            Arc::clone(&pool),
                            index,
                            workers,
                            opcache,
                            max_requests,
                        );
                        match mode {
                            Mode::Classic => worker.run_classic(&entrypoint),
                            Mode::Worker | Mode::Dispatcher => {
                                worker.run_resident(&entrypoint, settings.max_boot_failures);
                            }
                        }
                        pool.record_worker(index, Event::Stopped);
                        drop(guard);
                        // SAFETY: the last request ended and the watchdog released the PHP globals.
                        unsafe { rapira_thread_end() };
                    }));
                    if let Err(cause) = result {
                        let message = cause
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| cause.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("unknown panic");
                        error!(target: "rapira", "{name} pool: worker {index} panicked: {message}; exiting with code 101");
                        crate::exit::exit(101);
                    }
                })
                .with_context(|| format!("spawning {name} worker {index}"))?;
        }
        if !settings.request_terminate_timeout.is_zero() && self.watchdog.is_none() {
            self.watchdog = Some(crate::watchdog::start().context("starting the watchdog")?);
        }
        Ok(intake)
    }
}

/// # Safety
/// Requires an active request. `php_execute_script` catches a bailout itself.
pub(crate) unsafe fn run_script(script: &CStr) -> bool {
    unsafe {
        let mut fh: zend_file_handle = std::mem::zeroed();
        zend_stream_init_filename(&mut fh, script.as_ptr());
        fh.primary_script = true;
        let ok: bool = php_execute_script(&mut fh);
        zend_destroy_file_handle(&mut fh);
        ok
    }
}
