use std::path::Path;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use rapira_config::PoolSettings;
use rapira_master::{WORKER_EXIT_RECYCLE, WORKER_EXIT_UNHEALTHY, WorkerEnv};
use rapira_sapi::Rapira;
use rapira_sapi::plugin::{Plugin, Stopper, run_plugin};
use rapira_sapi::quota::{STOP_QUOTA, STOP_REASON, STOP_UNHEALTHY};

/// Jitter avoids lockstep recycling. The hash mixes in the pid because every child inherits the same seed from the pre-fork master.
fn effective_quota(max_requests: u64) -> u64 {
    if max_requests == 0 {
        return 0;
    }
    let grace = (max_requests / 2).max(1);
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    max_requests.saturating_add(1 + (h.finish() % grace))
}

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
    // The worker enters the entrypoint directory once and owns it from here; PHP keeps it over every script run.
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
    let (rapira, sink) = Rapira::start_worker(
        mode,
        entrypoint,
        effective_quota(max_requests),
        env.slot_view,
        stopper.clone(),
        plugin.dispatcher(),
    );

    rapira_master::spawn_lifeline_watch(env.lifeline);

    let name: &str = plugin.name();
    let outcome: anyhow::Result<()> = run_plugin(plugin, sink, stopper.clone(), grace, drain_grace)
        .and_then(|running| {
            spawn_signal_thread(move || stopper.stop());
            running.join()
        });
    if let Err(e) = &outcome {
        tracing::error!(target: "rapira", "plugin {name}: {e:#}");
    }
    drop(rapira);

    match STOP_REASON.load(SeqCst) {
        STOP_QUOTA => WORKER_EXIT_RECYCLE,
        STOP_UNHEALTHY => WORKER_EXIT_UNHEALTHY,
        _ if outcome.is_err() => 1,
        _ => 0,
    }
}

/// Requires the fork bracket to have masked exactly {QUIT, INT} in the child: the first signal runs `stop`, a second force-exits 131.
pub fn spawn_signal_thread(stop: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name("rapira-worker-signal".into())
        .spawn(move || {
            let sig = rapira_master::wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
            tracing::info!(target: "rapira", "signal {sig} received; draining worker");
            stop();
            let _ = rapira_master::wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
            tracing::warn!(target: "rapira", "second signal; forcing worker exit");
            std::process::exit(131);
        })
        .expect("spawn worker signal thread");
}
