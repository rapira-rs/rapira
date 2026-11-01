use std::sync::atomic::Ordering::Release;
use std::time::Duration;

use anyhow::Context;
use rapira_master::{WORKER_EXIT_DRAINED, WorkerEnv};

use crate::PoolRun;

/// Binds the observability listener and packs what the master forks with.
pub fn pool_run(settings: rapira_observability::config::Settings) -> anyhow::Result<PoolRun> {
    let build = rapira_observability::Build {
        version: env!("CARGO_PKG_VERSION"),
        php_version: rapira_sapi::linked_php_version(),
    };
    let server = rapira_observability::Server::new(settings, build)
        .context("observability: prepare failed")?;
    Ok(PoolRun::Observability { server })
}

/// Returns the process exit code for the master's fork bracket. The process runs no PHP.
pub fn observability_body(
    env: WorkerEnv,
    server: rapira_observability::Server,
    drain_grace: Duration,
) -> i32 {
    // The process name identifies the observability process: https://man7.org/linux/man-pages/man2/PR_SET_NAME.2const.html
    #[cfg(target_os = "linux")]
    // SAFETY: prctl reads a NUL-terminated static string that fits the 16-byte limit.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"rapira-obs".as_ptr())
    };
    // The observability process never pulls PHP work, so it reports idle itself.
    env.slot_view
        .state
        .store(rapira_scoreboard::SLOT_IDLE, Release);
    rapira_master::spawn_lifeline_watch(env.lifeline);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // The master sends a reload to every pool, and this process has no app to restart.
    crate::worker::spawn_signal_thread(
        move || {
            stop_tx.send_replace(true);
        },
        || {},
    );
    // main.rs puts the observability pool first, so the other pools follow it.
    match server.serve(env.board, &env.regions[1..], stop_rx, drain_grace) {
        Ok(()) => WORKER_EXIT_DRAINED,
        Err(e) => {
            tracing::error!(target: "observability", "{e:#}");
            1
        }
    }
}
