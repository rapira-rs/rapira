use std::time::Duration;

use anyhow::Context;
use rapira_master::{PoolConfig, WorkerEnv};
use rapira_net::PrepareCtx;

use crate::PoolRun;

/// The metrics pool: one process without PHP and without a request timeout. The listener is bound here, before the fork.
pub fn pool_run(
    settings: rapira_metrics::config::Settings,
    prepare: &mut PrepareCtx,
) -> anyhow::Result<(PoolRun, PoolConfig)> {
    let build = rapira_metrics::Build {
        version: env!("CARGO_PKG_VERSION"),
        php_version: rapira_sapi::linked_php_version(),
    };
    let server =
        rapira_metrics::Server::new(settings, build, prepare).context("metrics: prepare failed")?;
    let pool = PoolConfig {
        name: "metrics",
        processes: 1,
        request_terminate_timeout: Duration::ZERO,
    };
    Ok((
        PoolRun::Metrics {
            server: Some(server),
        },
        pool,
    ))
}

/// Returns the process exit code for the master's fork bracket. The process runs no PHP.
pub fn metrics_body(env: WorkerEnv, server: rapira_metrics::Server, drain_grace: Duration) -> i32 {
    // The process name identifies the metrics process: https://man7.org/linux/man-pages/man2/PR_SET_NAME.2const.html
    #[cfg(target_os = "linux")]
    // SAFETY: prctl reads a NUL-terminated static string that fits the 16-byte limit.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"rapira-metrics".as_ptr())
    };
    env.slot_view.bind(std::process::id());
    rapira_master::spawn_lifeline_watch(env.lifeline);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    crate::worker::spawn_signal_thread(move || {
        stop_tx.send_replace(true);
    });
    match server.serve(env.board, env.regions, env.pool, stop_rx, drain_grace) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(target: "metrics", "{e:#}");
            1
        }
    }
}
