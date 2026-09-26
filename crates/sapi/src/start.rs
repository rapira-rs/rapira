use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    pending: Arc<AtomicUsize>,
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
    board: Option<rapira_scoreboard::Scoreboard>,
    module: Option<PhpModule>,
}

/// Split a `PHP_VERSION_ID` (major * 10000 + minor * 100 + patch) into major and minor: https://www.php.net/manual/en/function.phpversion.php
fn php_series(id: u32) -> (u32, u32) {
    (id / 10_000, (id / 100) % 100)
}

/// Zend structs are bound by bindgen at build time, so a libphp from another PHP minor is an ABI mismatch (`sapi_startup` handed a differently shaped struct), not a load error.
fn check_linked_php() -> anyhow::Result<()> {
    // SAFETY: php_version_id() returns a compile-time constant and touches no engine state, so this is valid pre-startup.
    let linked = unsafe { php_version_id() };
    let (want, got) = (php_series(PHP_VERSION_ID), php_series(linked));
    anyhow::ensure!(
        want == got,
        "linked libphp is PHP {}.{}, but this rapira was built against PHP {}.{}. \
         Use a libphp from the same PHP minor as the build.",
        got.0,
        got.1,
        want.0,
        want.1
    );
    Ok(())
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

    if !started {
        error!(target: "rapira", "php_module_startup failed, shutting down");
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
        return Err(anyhow::anyhow!("php_module_startup failed"));
    }
    Ok(PhpModule {})
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
    /// `entrypoint`: the script of every request in classic mode, the worker script otherwise. `classes`: the dispatcher surface receive() serves; None for the classic and worker modes.
    pub fn start_worker(
        mode: Mode,
        entrypoint: PathBuf,
        hooks: WorkerHooks,
        classes: Option<DispatcherClasses>,
    ) -> anyhow::Result<Self> {
        let WorkerHooks {
            max_requests,
            on_quota,
            on_unhealthy,
            slot,
            on_thread_start,
        } = hooks;
        let (board, slot) = match slot {
            Some(s) => (None, s),
            None => {
                let board = rapira_scoreboard::Scoreboard::create(1)?;
                (Some(board), board.slot(0))
            }
        };
        slot.bind(std::process::id());
        let pending = Arc::new(AtomicUsize::new(0));
        let (intake_tx, intake_rx) = sync_channel::<Box<dyn Work>>(1024);
        let sink = Sink::new(intake_tx, pending.clone());

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
                on_thread_start,
            )
        });

        Ok(Self {
            sink: Some(sink),
            worker: Some(worker),
            board,
            module: None,
        })
    }

    pub fn start(
        parts: &[PhpPart],
        mode: Mode,
        entrypoint: PathBuf,
        classes: Option<DispatcherClasses>,
    ) -> anyhow::Result<Self> {
        Self::start_with_hooks(parts, mode, entrypoint, WorkerHooks::default(), classes)
    }

    /// Boots the master and one worker in this process.
    pub fn start_with_hooks(
        parts: &[PhpPart],
        mode: Mode,
        entrypoint: PathBuf,
        hooks: WorkerHooks,
        classes: Option<DispatcherClasses>,
    ) -> anyhow::Result<Self> {
        info!(target: "rapira", "booting with mode: {mode:?}");
        let module = boot_master(parts)?;
        let mut rapira = Self::start_worker(mode, entrypoint, hooks, classes)?;
        rapira.module = Some(module);
        Ok(rapira)
    }

    /// The intake of this worker. The PHP thread sees the intake closed once `Rapira` and every clone are dropped.
    pub fn sink(&self) -> Sink {
        self.sink.clone().expect("the sink lives until Drop")
    }

    /// The slot of the private one-slot board. It is `None` when the master owns the slot.
    pub fn scoreboard(&self) -> Option<rapira_scoreboard::SlotSnapshot> {
        self.board?.snapshot_slots().pop()
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!(target: "rapira", "shutting down, dropping");
        self.sink = None;
        let Some(worker) = self.worker.take() else {
            std::mem::forget(self.module.take());
            return;
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !worker.is_finished() {
            error!(
                target: "rapira",
                "worker still running after grace; skipping PHP module shutdown to avoid UB on a live thread"
            );
            std::mem::forget(self.module.take());
            return;
        }

        let _ = worker.join();
        drop(self.module.take());
    }
}

/// NTS inits module and request on different threads, so the call stack is re-initialized on this thread: https://github.com/php/php-src/pull/9104
fn worker_main(
    mode: Mode,
    entrypoint: PathBuf,
    rx: JobRx,
    classes: Option<DispatcherClasses>,
    on_thread_start: Option<Box<dyn FnOnce() + Send>>,
) {
    JOB_RX.with_borrow_mut(|slot| *slot = Some(rx));
    crate::exchange::set_classes(classes);
    if let Some(f) = on_thread_start {
        f();
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
    JOB_RX.with_borrow_mut(|slot| {
        let Some(job_r) = slot.as_mut() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let got = match timeout {
            None => job_r.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(t) => job_r.rx.recv_timeout(t),
        };
        sb_update(Event::Active);
        match got {
            Ok(job) => {
                job_r.pending.fetch_sub(1, Ordering::Relaxed);
                Pulled::Job(job)
            }
            Err(RecvTimeoutError::Timeout) => Pulled::Timeout,
            Err(RecvTimeoutError::Disconnected) => Pulled::Closed,
        }
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

pub(crate) fn pending_depth() -> usize {
    JOB_RX.with_borrow(|slot| {
        slot.as_ref()
            .map_or(0, |job_r| job_r.pending.load(Ordering::Relaxed))
    })
}

#[cfg(test)]
mod tests {
    use super::php_series;

    #[test]
    fn php_series_drops_the_patch() {
        assert_eq!(php_series(80_508), (8, 5));
        assert_eq!(php_series(80_426), (8, 4));
        assert_eq!(php_series(80_500), php_series(80_599));
        assert_ne!(php_series(80_400), php_series(80_500));
    }
}
