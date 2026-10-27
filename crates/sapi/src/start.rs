use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, sync_channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{error, info, trace};

use crate::quota;
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, sb_set, sb_update};
use crate::work::{DispatcherClasses, Entry, Intake, Work};
use crate::{
    classic_worker::classic_worker,
    plugin::{Mode, Stopper},
    *,
};

thread_local! {
    static UNIT_RX: RefCell<Option<UnitRx>> = const { RefCell::new(None) };
}

struct UnitRx {
    rx: Receiver<Entry>,
    /// The `pending` field of this worker's scoreboard slot.
    pending: &'static AtomicU64,
}

/// The idle wait after which the PHP thread calls `mi_collect`. mimalloc purges freed arena slices only from a page free or a collect, so an idle worker keeps the slices of the last burst committed.
const IDLE_PURGE_AFTER: Duration = Duration::from_secs(2);

impl UnitRx {
    /// A pulled unit leaves `pending` and gives back its body bytes.
    fn pulled(&self, entry: Entry) -> Pulled {
        match entry {
            Entry::Unit { unit, .. } => {
                self.pending.fetch_sub(1, Ordering::Relaxed);
                Pulled::Unit(unit)
            }
            Entry::RestartApp => Pulled::RestartApp,
        }
    }

    /// Waits for one entry, with no time limit when `timeout` is None.
    fn recv(&self, timeout: Option<Duration>) -> Pulled {
        let first = timeout.map_or(IDLE_PURGE_AFTER, |t| t.min(IDLE_PURGE_AFTER));
        let mut got = self.rx.recv_timeout(first);
        if matches!(got, Err(RecvTimeoutError::Timeout)) && first == IDLE_PURGE_AFTER {
            // SAFETY: plain allocator call on this thread; the arena purge is process-wide and locked inside mimalloc.
            unsafe { libmimalloc_sys::mi_collect(true) };
            got = match timeout {
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(t) => self.rx.recv_timeout(t - first),
            };
        }
        match got {
            Ok(entry) => self.pulled(entry),
            Err(RecvTimeoutError::Timeout) => Pulled::Timeout,
            Err(RecvTimeoutError::Disconnected) => Pulled::Closed,
        }
    }
}

thread_local! {
    /// The register functions that MINIT calls after the base classes. MINIT runs on the thread that calls `boot_master`.
    static REGISTER_FNS: RefCell<Vec<unsafe extern "C" fn()>> = const { RefCell::new(Vec::new()) };
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
    worker: Option<JoinHandle<()>>,
}

/// The PHP thread of one worker and its intake.
pub struct StartedWorker {
    pub rapira: Rapira,
    /// The intake of this worker. The PHP thread sees it closed once every clone is dropped.
    pub intake: Intake,
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

/// MINIT once in the master. Base classes first, then each register function in order.
pub fn boot_master(register: &[unsafe extern "C" fn()]) -> anyhow::Result<PhpModule> {
    check_linked_php()?;
    REGISTER_FNS.set(register.to_vec());
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
    unsafe { rapira_register_post_reader() };
    Ok(module)
}

/// MINIT calls it after the base classes (module.c).
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_register_plugin_classes() {
    REGISTER_FNS.with_borrow(|register| {
        for f in register {
            // SAFETY: MINIT runs on the booting thread, and the base classes the plugin classes extend are registered.
            unsafe { f() };
        }
    });
}

impl Rapira {
    /// `entrypoint`: the script of every request in classic mode, the worker script otherwise. `max_requests`: 0 = unlimited; jitter already applied by the caller.
    /// `slot`: the scoreboard slot this worker reports into. `stopper` stops the plugin when PHP turns unhealthy. `classes`: the dispatcher surface receive() serves in dispatcher mode.
    pub fn start_worker(
        mode: Mode,
        entrypoint: PathBuf,
        max_requests: u64,
        slot: &'static rapira_scoreboard::SharedSlot,
        stopper: Stopper,
        classes: DispatcherClasses,
    ) -> StartedWorker {
        let pending: &'static AtomicU64 = &slot.pending;
        let (intake_tx, intake_rx) = sync_channel::<Entry>(1024);
        let intake = Intake::new(intake_tx, slot);

        crate::context::set_script(&entrypoint);
        // SAFETY: written once, before the PHP thread that reads it exists.
        unsafe {
            crate::rapira_mode = match mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker => RAPIRA_MODE_WORKER,
                Mode::Dispatcher => RAPIRA_MODE_DISPATCHER,
            } as c_int;
        };

        trace!(target: "rapira", "spawning worker thread");
        // PHP sets its stack limit from the stack size of this thread. 8 MiB matches the CLI and FPM main thread under the default RLIMIT_STACK. https://github.com/php/php-src/blob/php-8.5.11/Zend/zend_call_stack.c#L129-L173
        let worker: JoinHandle<()> = thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                sb_set(slot);
                quota::install(max_requests, stopper);
                worker_main(
                    mode,
                    entrypoint,
                    UnitRx {
                        rx: intake_rx,
                        pending,
                    },
                    classes,
                )
            })
            .expect("spawn the PHP thread");

        StartedWorker {
            rapira: Self {
                worker: Some(worker),
            },
            intake,
        }
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!(target: "rapira", "shutting down, dropping");
        let Some(worker) = self.worker.take() else {
            return;
        };

        // The PHP thread ends once every Intake clone is dropped. The caller drops the Intake, or the plugin that holds it, before Rapira.
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
fn worker_main(mode: Mode, entrypoint: PathBuf, rx: UnitRx, classes: DispatcherClasses) {
    UNIT_RX.with_borrow_mut(|slot| *slot = Some(rx));
    if mode == Mode::Dispatcher {
        crate::dispatcher::set_classes(classes);
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

pub(crate) fn pull_unit() -> Option<Box<dyn Work>> {
    match pull_unit_wait(None) {
        Pulled::Unit(unit) => Some(unit),
        _ => None,
    }
}

pub(crate) enum Pulled {
    Unit(Box<dyn Work>),
    /// A reload: the app instance takes no more units.
    RestartApp,
    Timeout,
    Empty,
    Closed,
}

/// Idle covers only the park: control returns to PHP as Active on every arm, or the master watchdog skips a worker spinning after a no-unit return.
pub(crate) fn pull_unit_wait(timeout: Option<Duration>) -> Pulled {
    UNIT_RX.with_borrow(|slot| {
        let Some(unit_r) = slot.as_ref() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let pulled = unit_r.recv(timeout);
        sb_update(Event::Active);
        pulled
    })
}

/// The host pulls a unit to shed it after a failed boot cycle, and waits at most `timeout`. Before this pull, the worker stored starting, or draining after the drain decision. The pull keeps the state.
pub(crate) fn pull_unit_to_shed(timeout: Duration) -> Pulled {
    UNIT_RX.with_borrow(|slot| match slot.as_ref() {
        Some(unit_r) => unit_r.recv(Some(timeout)),
        None => Pulled::Closed,
    })
}

/// The Idle/Active pair still runs: a polling worker must refresh last_activity_ms or the master watchdog TERMs it as a stuck request (`Pool::watchdog_tick`).
pub(crate) fn pull_unit_try() -> Pulled {
    UNIT_RX.with_borrow_mut(|slot| {
        let Some(unit_r) = slot.as_mut() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let got = unit_r.rx.try_recv();
        sb_update(Event::Active);
        match got {
            Ok(entry) => unit_r.pulled(entry),
            Err(TryRecvError::Empty) => Pulled::Empty,
            Err(TryRecvError::Disconnected) => Pulled::Closed,
        }
    })
}

pub(crate) fn pending_depth() -> u64 {
    UNIT_RX.with_borrow(|slot| {
        slot.as_ref()
            .map_or(0, |unit_r| unit_r.pending.load(Ordering::Relaxed))
    })
}
