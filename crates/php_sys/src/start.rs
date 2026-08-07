use std::cell::RefCell;
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::{error, info, trace};
use types::Job;

use crate::quota::{self, WorkerHooks};
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, ScoreboardSnapshot, sb_set, sb_update};
use crate::{classic_worker::classic_worker, types::Mode, *};

thread_local! {
    static JOB_RX: RefCell<Option<Receiver<Job>>> = const { RefCell::new(None) };
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
    pub(crate) intake: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
    board: Option<rapira_scoreboard::Scoreboard>,
    module: Option<PhpModule>,
}

impl Rapira {
    pub fn boot_master() -> anyhow::Result<PhpModule> {
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

    // worker side part
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
                (Some(board), board.slot(0).expect("slot 0 exists"))
            }
        };
        slot.bind(std::process::id());

        let (intake, intake_rx) = mpsc::channel::<Job>(1024);

        // SAFETY: safe, trust me, I'm a developer
        unsafe {
            crate::rapira_mode = match &mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::WorkerRequest(_) => RAPIRA_MODE_WORKER_REQUEST,
            } as c_int;
        };

        trace!(target: "rapira", "spawning worker thread");
        let worker: JoinHandle<()> = thread::spawn(move || {
            sb_set(slot);
            quota::install(max_requests, on_quota, on_unhealthy);
            worker_main(mode, intake_rx)
        });

        Ok(Self {
            intake: Some(intake),
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

    pub fn shutdown(self) {
        info!(target: "rapira", "shutdown is noop, deinitialize in Drop");
    }

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

        // https://www.php.net/manual/en/info.configuration.php#ini.max-execution-time
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !worker.is_finished() {
            error!(
                target: "rapira",
                "worker still running after grace; skipping PHP module shutdown to avoid UB on a live thread"
            );
            // Leak the module teardown; process exit reclaims it.
            std::mem::forget(self.module.take());
            return;
        }

        let _ = worker.join();
        // Fused: module teardown, same order as before the split; worker flavor: no-op.
        drop(self.module.take());
    }
}

fn worker_main(mode: Mode, rx: Receiver<Job>) {
    JOB_RX.with_borrow_mut(|slot| *slot = Some(rx));
    loop {
        unsafe {
            // https://github.com/php/php-src/pull/9104
            // NTS inits module and request on different threads, so the call stack must be
            // re-initialized here.
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match &mode {
            Mode::Classic => {
                classic_worker();
                WorkerExit::Closed
            }
            Mode::WorkerRequest(script) => rapira_worker(script.clone()),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
    }
}

pub(crate) fn pull_job() -> Option<Job> {
    JOB_RX.with_borrow_mut(|rx| {
        let rx = rx.as_mut()?;
        sb_update(Event::Idle);
        let job = rx.blocking_recv();
        if job.is_some() {
            sb_update(Event::Active);
        }
        job
    })
}
