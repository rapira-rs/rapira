use crate::work::{Queued, Work};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{error, info, trace};

use crate::quota::{self, WorkerHooks};
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, ScoreboardSnapshot, sb_set, sb_update};
use crate::{classic_worker::classic_worker, types::Mode, *};

thread_local! {
    static JOB_RX: RefCell<Option<JobRx>> = const { RefCell::new(None) };
}

pub(crate) struct Intake {
    pub(crate) tx: SyncSender<Queued>,
    pub(crate) pending: Arc<AtomicUsize>,
}

struct JobRx {
    rx: Receiver<Queued>,
    pending: Arc<AtomicUsize>,
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
    pub(crate) intake: Option<Intake>,
    pub(crate) dispatcher: bool,
    pub(crate) grpc: bool,
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
    // SAFETY: both accessors read a compile-time constant and touch no engine state, so this is valid pre-startup.
    let (headers, linked) = unsafe { (rapira_headers_php_version_id(), php_version_id()) };
    let (want, got) = (php_series(headers), php_series(linked));
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

impl Rapira {
    pub fn boot_master() -> anyhow::Result<PhpModule> {
        check_linked_php()?;
        let mut module: _sapi_module_struct = module::build_sapi_module();
        let started: bool = unsafe {
            rapira_process_init();
            sapi_startup(&mut module);
            module
                .startup
                .is_some_and(|start| start(&mut module) == SUCCESS)
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

    pub fn start_worker(mode: Mode, hooks: WorkerHooks) -> anyhow::Result<Self> {
        let WorkerHooks {
            max_requests,
            on_quota,
            on_unhealthy,
            slot,
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
        let (intake_tx, intake_rx) = sync_channel::<Queued>(1024);
        let intake = Intake {
            tx: intake_tx,
            pending: pending.clone(),
        };

        let grpc = matches!(mode, Mode::GrpcDispatcher { .. });
        let dispatcher = matches!(mode, Mode::Dispatcher(_) | Mode::GrpcDispatcher { .. });
        // SAFETY: The PHP worker thread starts after this write.
        unsafe {
            crate::rapira_mode = match &mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker(_) => RAPIRA_MODE_WORKER,
                Mode::Dispatcher(_) | Mode::GrpcDispatcher { .. } => RAPIRA_MODE_DISPATCHER,
            } as c_int;
        };

        trace!(target: "rapira", "spawning worker thread");
        let worker: JoinHandle<()> = thread::spawn(move || {
            sb_set(slot);
            quota::install(max_requests, on_quota, on_unhealthy);
            worker_main(
                mode,
                JobRx {
                    rx: intake_rx,
                    pending,
                },
            )
        });

        Ok(Self {
            intake: Some(intake),
            dispatcher,
            grpc,
            worker: Some(worker),
            board,
            module: None,
        })
    }

    pub fn start(mode: Mode) -> anyhow::Result<Self> {
        info!(target: "rapira", "booting with mode: {mode:?}");
        let module = Self::boot_master()?;
        let mut rapira = Self::start_worker(mode, WorkerHooks::default())?;
        rapira.module = Some(module);
        Ok(rapira)
    }

    pub fn shutdown(self) {}

    pub fn scoreboard(&self) -> ScoreboardSnapshot {
        match &self.board {
            Some(board) => crate::scoreboard::snapshot(board),
            None => ScoreboardSnapshot::default(),
        }
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!(target: "rapira", "shutting down, dropping");
        self.intake = None;
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
fn worker_main(mode: Mode, rx: JobRx) {
    let (entrypoint, services) = match mode {
        Mode::Classic => (None, None),
        Mode::Worker(entrypoint) | Mode::Dispatcher(entrypoint) => (Some(entrypoint), None),
        Mode::GrpcDispatcher {
            entrypoint,
            services,
        } => (Some(entrypoint), Some(services)),
    };
    JOB_RX.with_borrow_mut(|slot| *slot = Some(rx));
    crate::grpc::install(services);
    loop {
        unsafe {
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match &entrypoint {
            None => {
                classic_worker();
                WorkerExit::Closed
            }
            Some(script) => rapira_worker(script.clone()),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
    }
    JOB_RX.with_borrow_mut(|slot| *slot = None);
}

pub(crate) fn pull_job() -> Option<Work> {
    match pull_job_wait(None) {
        Pulled::Job(job) => Some(job),
        _ => None,
    }
}

pub(crate) enum Pulled {
    Job(Work),
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
            Ok(job) => Pulled::Job(job.work),
            Err(RecvTimeoutError::Timeout) => Pulled::Timeout,
            Err(RecvTimeoutError::Disconnected) => Pulled::Closed,
        }
    })
}

/// The Idle/Active pair still runs: a polling worker must refresh last_activity_ms or the master watchdog TERMs it as a stuck request (master/src/events.rs:509-517).
pub(crate) fn pull_job_try() -> Pulled {
    JOB_RX.with_borrow_mut(|slot| {
        let Some(job_r) = slot.as_mut() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let got = job_r.rx.try_recv();
        sb_update(Event::Active);
        match got {
            Ok(job) => Pulled::Job(job.work),
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

    #[test]
    fn receive_budget_includes_discard_cleanup() {
        use super::*;
        use crate::work::{PendingGuard, Queued, ReceiveWait, Work};

        struct SlowDrop;
        impl AsRef<[u8]> for SlowDrop {
            fn as_ref(&self) -> &[u8] {
                b""
            }
        }
        impl Drop for SlowDrop {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let pending = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = sync_channel(2);
        let (cancelled, receiver) = tokio::sync::oneshot::channel();
        drop(receiver);
        let (live, _receiver) = tokio::sync::oneshot::channel();
        struct Case {
            name: &'static str,
            sender: tokio::sync::oneshot::Sender<crate::grpc::Reply>,
            message: bytes::Bytes,
        }
        let cases = [
            Case {
                name: "cancelled",
                sender: cancelled,
                message: bytes::Bytes::from_owner(SlowDrop),
            },
            Case {
                name: "live",
                sender: live,
                message: bytes::Bytes::new(),
            },
        ];
        for case in cases {
            let job = crate::grpc::Job {
                request: crate::grpc::Request {
                    method: case.name.into(),
                    message: case.message,
                    metadata: Vec::new(),
                    remote: crate::types::Addr::Unix(None),
                    tls: None,
                    received_at: 0.0,
                    deadline: None,
                    expires_at: None,
                },
                sender: case.sender,
            };
            assert!(
                tx.send(Queued {
                    work: Work::Grpc(Box::new(job)),
                    _pending: PendingGuard::arm(&pending)
                })
                .is_ok()
            );
        }
        JOB_RX.with_borrow_mut(|slot| {
            *slot = Some(JobRx {
                rx,
                pending: pending.clone(),
            })
        });
        assert!(matches!(ReceiveWait::new(50_000).pull(), Pulled::Timeout));
        assert_eq!(
            pending.load(Ordering::Relaxed),
            1,
            "the live job stays queued after the budget expires"
        );
        JOB_RX.with_borrow_mut(|slot| *slot = None);
        assert_eq!(pending.load(Ordering::Relaxed), 0);
    }
}
