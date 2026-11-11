use std::path::Path;
use std::time::Duration;

use rapira_config::PoolSettings;
use rapira_master::{WORKER_EXIT_DRAINED, WORKER_EXIT_UNHEALTHY, WorkerEnv};
use rapira_sapi::plugin::{Plugin, Stopper, run_plugin};
use rapira_sapi::{Rapira, StartedWorker};

/// Returns the process exit code for the master's fork bracket; never runs PHP module teardown, MSHUTDOWN stays with the master.
pub fn worker_body(
    env: WorkerEnv,
    plugin: Box<dyn Plugin>,
    pool: PoolSettings,
    grace: Duration,
    drain_grace: Duration,
) -> i32 {
    let PoolSettings {
        mode,
        entrypoint,
        max_requests,
        ..
    } = pool;
    // SAFETY: single-threaded here, before the PHP worker thread exists.
    unsafe { rapira_sapi::rapira_child_init() };
    // A start check: a worker that cannot enter the entrypoint directory exits. Each app start and each classic reload enter the directory again.
    let entrypoint_dir: &Path = entrypoint
        .parent()
        .expect("the boot check accepted a regular file");
    if let Err(e) = std::env::set_current_dir(entrypoint_dir) {
        tracing::error!(
            target: "rapira",
            "entering the entrypoint directory {}: {e}",
            entrypoint_dir.display()
        );
        return WORKER_EXIT_UNHEALTHY;
    }
    let stopper = Stopper::default();
    let StartedWorker { rapira, intake } = Rapira::start_worker(
        mode,
        entrypoint,
        max_requests,
        env.slot_view,
        stopper.clone(),
        plugin.as_ref(),
    );

    rapira_master::spawn_lifeline_watch(env.lifeline);

    let name: &str = plugin.name();
    let outcome: anyhow::Result<()> =
        run_plugin(plugin, intake, stopper.clone(), grace, drain_grace).and_then(|running| {
            let restarter = stopper.clone();
            spawn_signal_thread(move || stopper.stop(), move || restarter.restart_app());
            running.join()
        });
    if let Err(e) = &outcome {
        tracing::error!(target: "rapira", "plugin {name}: {e:#}");
    }
    let unhealthy = rapira.join();

    if unhealthy {
        WORKER_EXIT_UNHEALTHY
    } else if outcome.is_err() {
        1
    } else {
        WORKER_EXIT_DRAINED
    }
}

/// Accepts the worker signals that the fork bracket left blocked: USR2 runs `restart`, the first QUIT or INT runs `stop`, a second force-exits 131.
pub fn spawn_signal_thread(
    stop: impl FnOnce() + Send + 'static,
    restart: impl Fn() + Send + 'static,
) {
    std::thread::Builder::new()
        .name("rapira-worker-signal".into())
        .spawn(move || {
            let sig = loop {
                match rapira_master::wait_worker_signal() {
                    libc::SIGUSR2 => restart(),
                    sig => break sig,
                }
            };
            tracing::info!(target: "rapira", "signal {sig} received; draining worker");
            stop();
            while rapira_master::wait_worker_signal() == libc::SIGUSR2 {}
            tracing::warn!(target: "rapira", "second signal; forcing worker exit");
            std::process::exit(131);
        })
        .expect("spawn worker signal thread");
}
