use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, sync_channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{error, info, trace};

use crate::quota::{self, WorkerHooks};
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, sb_set, sb_update};
use crate::work::{DispatcherClasses, Sink, Work};
use crate::{
    classic_worker::classic_worker,
    plugin::{Mode, PhpPart},
    *,
};

thread_local! {
    static JOB_RX: RefCell<Option<JobRx>> = const { RefCell::new(None) };
}

struct JobRx {
    rx: Receiver<Box<dyn Work>>,
    /// The `pending` field of this worker's scoreboard slot.
    pending: &'static AtomicU64,
}

impl JobRx {
    /// Waits for one unit, with no time limit when `timeout` is None. A pulled unit leaves `pending`.
    fn recv(&self, timeout: Option<Duration>) -> Pulled {
        let got = match timeout {
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(t) => self.rx.recv_timeout(t),
        };
        match got {
            Ok(job) => {
                self.pending.fetch_sub(1, Ordering::Relaxed);
                Pulled::Job(job)
            }
            Err(RecvTimeoutError::Timeout) => Pulled::Timeout,
            Err(RecvTimeoutError::Disconnected) => Pulled::Closed,
        }
    }
}

thread_local! {
    /// The parts that MINIT registers after the base classes. MINIT runs on the thread that calls `boot_master`.
    static PARTS: RefCell<Vec<PhpPart>> = const { RefCell::new(Vec::new()) };
}

pub struct PhpModule {}

impl Drop for PhpModule {
    fn drop(&mut self) {
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
    }
}

pub struct Rapira {
    sink: Option<Sink>,
    worker: Option<JoinHandle<()>>,
}

/// Zend structs are bound by bindgen at build time, so a libphp from another PHP minor is an ABI mismatch (`sapi_startup` handed a differently shaped struct), not a load error.
fn check_linked_php() -> anyhow::Result<()> {
    // SAFETY: php_version_id() returns a compile-time constant and touches no engine state, so this is valid pre-startup.
    let linked = unsafe { php_version_id() };
    // A PHP_VERSION_ID is major * 10000 + minor * 100 + patch, so `/ 100` drops the patch: https://www.php.net/manual/en/function.phpversion.php
    anyhow::ensure!(
        linked / 100 == PHP_VERSION_ID / 100,
        "linked libphp is PHP {} (PHP_VERSION_ID {linked}), but this rapira was built against PHP_VERSION_ID {PHP_VERSION_ID}. \
         Use a libphp from the same PHP minor as the build.",
        linked_php_version()
    );
    Ok(())
}

/// The PHP_VERSION string of the linked libphp.
pub fn linked_php_version() -> String {
    // SAFETY: php_version() returns a static NUL-terminated string and touches no engine state, so this is valid pre-startup.
    unsafe { CStr::from_ptr(php_version()) }
        .to_string_lossy()
        .into_owned()
}

/// MINIT once in the master. Base classes first, then each part in order.
pub fn boot_master(parts: &[PhpPart]) -> anyhow::Result<PhpModule> {
    check_linked_php()?;
    PARTS.set(parts.to_vec());
    let mut module: _sapi_module_struct = module::build_sapi_module();
    let started: bool = unsafe {
        // The Rust runtime sets SIGPIPE to SIG_IGN before main, so a write to a closed peer returns EPIPE: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/on-broken-pipe.html
        rapira_process_init();
        sapi_startup(&mut module);
        php_module_startup(&mut module, &raw mut rapira_module_entry) == SUCCESS
    };

    // Drop shuts the module down on the failure path too.
    let module = PhpModule {};
    if !started {
        error!(target: "rapira", "php_module_startup failed, shutting down");
        return Err(anyhow::anyhow!("php_module_startup failed"));
    }
    Ok(module)
}

/// MINIT calls it after the base classes (module.c).
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_register_plugin_classes() {
    PARTS.with_borrow(|parts| {
        for part in parts {
            // SAFETY: MINIT runs on the booting thread, and the base classes the part extends are registered.
            unsafe { (part.register)() };
        }
    });
}

impl Rapira {
    /// `entrypoint`: the script of every request in classic mode, the worker script otherwise. `classes`: the dispatcher surface receive() serves in dispatcher mode.
    pub fn start_worker(
        mode: Mode,
        entrypoint: PathBuf,
        hooks: WorkerHooks,
        classes: DispatcherClasses,
    ) -> Self {
        let WorkerHooks {
            max_requests,
            on_quota,
            on_unhealthy,
            slot,
        } = hooks;
        let pending: &'static AtomicU64 = &slot.pending;
        let (intake_tx, intake_rx) = sync_channel::<Box<dyn Work>>(1024);
        let sink = Sink::new(intake_tx, slot);

        crate::context::set_script(&entrypoint);
        // SAFETY: safe, trust me, I'm a developer
        unsafe {
            crate::rapira_mode = match mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker => RAPIRA_MODE_WORKER,
                Mode::Dispatcher => RAPIRA_MODE_DISPATCHER,
            } as c_int;
        };

        trace!(target: "rapira", "spawning worker thread");
        let worker: JoinHandle<()> = thread::spawn(move || {
            sb_set(slot);
            quota::install(max_requests, on_quota, on_unhealthy);
            worker_main(
                mode,
                entrypoint,
                JobRx {
                    rx: intake_rx,
                    pending,
                },
                classes,
            )
        });

        Self {
            sink: Some(sink),
            worker: Some(worker),
        }
    }

    /// The intake of this worker. The PHP thread sees the intake closed once `Rapira` and every clone are dropped.
    pub fn sink(&self) -> Sink {
        self.sink.clone().expect("the sink lives until Drop")
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!(target: "rapira", "shutting down, dropping");
        self.sink = None;
        let Some(worker) = self.worker.take() else {
            return;
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !worker.is_finished() {
            error!(target: "rapira", "worker still running after grace; leaving it unjoined");
            return;
        }

        let _ = worker.join();
    }
}

/// NTS inits module and request on different threads, so the call stack is re-initialized on this thread: https://github.com/php/php-src/pull/9104
fn worker_main(mode: Mode, entrypoint: PathBuf, rx: JobRx, classes: DispatcherClasses) {
    JOB_RX.with_borrow_mut(|slot| *slot = Some(rx));
    if mode == Mode::Dispatcher {
        crate::exchange::set_classes(classes);
    }
    loop {
        unsafe {
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match mode {
            Mode::Classic => {
                classic_worker();
                WorkerExit::Closed
            }
            Mode::Worker | Mode::Dispatcher => rapira_worker(entrypoint.clone()),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
    }
}

/// # Safety
/// Requires an active request. `php_execute_script` catches a bailout itself.
pub(crate) unsafe fn run_script(script: &Path) -> bool {
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

pub(crate) fn pull_job() -> Option<Box<dyn Work>> {
    match pull_job_wait(None) {
        Pulled::Job(job) => Some(job),
        _ => None,
    }
}

pub(crate) enum Pulled {
    Job(Box<dyn Work>),
    Timeout,
    Empty,
    Closed,
}

/// Idle covers only the park: control returns to PHP as Active on every arm, or the master watchdog skips a worker spinning after a no-unit return.
pub(crate) fn pull_job_wait(timeout: Option<Duration>) -> Pulled {
    JOB_RX.with_borrow(|slot| {
        let Some(job_r) = slot.as_ref() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let pulled = job_r.recv(timeout);
        sb_update(Event::Active);
        pulled
    })
}

/// The host pulls a unit to shed it after a failed boot cycle, and waits at most `timeout`. Before this pull, the worker stored starting, or draining after the drain decision. The pull keeps the state.
pub(crate) fn pull_job_to_shed(timeout: Duration) -> Pulled {
    JOB_RX.with_borrow(|slot| match slot.as_ref() {
        Some(job_r) => job_r.recv(Some(timeout)),
        None => Pulled::Closed,
    })
}

/// The Idle/Active pair still runs: a polling worker must refresh last_activity_ms or the master watchdog TERMs it as a stuck request (`Pool::watchdog_tick`).
pub(crate) fn pull_job_try() -> Pulled {
    JOB_RX.with_borrow_mut(|slot| {
        let Some(job_r) = slot.as_mut() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let got = job_r.rx.try_recv();
        sb_update(Event::Active);
        match got {
            Ok(job) => {
                job_r.pending.fetch_sub(1, Ordering::Relaxed);
                Pulled::Job(job)
            }
            Err(TryRecvError::Empty) => Pulled::Empty,
            Err(TryRecvError::Disconnected) => Pulled::Closed,
        }
    })
}

pub(crate) fn pending_depth() -> u64 {
    JOB_RX.with_borrow(|slot| {
        slot.as_ref()
            .map_or(0, |job_r| job_r.pending.load(Ordering::Relaxed))
    })
}
