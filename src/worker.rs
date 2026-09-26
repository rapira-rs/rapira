use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering::SeqCst};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rapira_master::{WORKER_EXIT_RECYCLE, WORKER_EXIT_UNHEALTHY, WorkerEnv};
use rapira_sapi::plugin::{Mode, Plugin, Stopper, run_plugin};
use rapira_sapi::work::DispatcherClasses;
use rapira_sapi::{Rapira, WorkerHooks};

/// First writer wins, except unhealthy upgrades a pending recycle; -1 = unset, so the plugin outcome sets the exit code.
static WORKER_EXIT: AtomicI32 = AtomicI32::new(-1);

/// Racing ahead of the stopper registration is fine: the boot path re-checks WORKER_EXIT right after registering.
fn request_worker_exit(code: i32, stopper: &OnceLock<Stopper>) {
    let decided = WORKER_EXIT
        .compare_exchange(-1, code, SeqCst, SeqCst)
        .is_ok()
        || (code == WORKER_EXIT_UNHEALTHY
            && WORKER_EXIT
                .compare_exchange(WORKER_EXIT_RECYCLE, WORKER_EXIT_UNHEALTHY, SeqCst, SeqCst)
                .is_ok());
    if decided && let Some(s) = stopper.get() {
        s.stop();
    }
}

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

/// Everything a pool's worker needs besides the fork-time env; cloned into each child.
#[derive(Clone)]
pub struct PoolArgs {
    pub mode: Mode,
    pub entrypoint: PathBuf,
    pub max_requests: u64,
    pub grace: Duration,
    pub drain_grace: Duration,
}

/// Returns the process exit code for the master's fork bracket; never runs PHP module teardown, MSHUTDOWN stays with the master.
pub fn worker_body(env: WorkerEnv, plugin: Box<dyn Plugin>, args: PoolArgs) -> i32 {
    let PoolArgs {
        mode,
        entrypoint,
        max_requests,
        grace,
        drain_grace,
    } = args;
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
    let classes: Option<DispatcherClasses> = if mode == Mode::Dispatcher {
        plugin.php().and_then(|p| p.dispatcher)
    } else {
        None
    };
    let stopper: Arc<OnceLock<Stopper>> = Arc::new(OnceLock::new());
    let hooks: WorkerHooks = WorkerHooks {
        max_requests: effective_quota(max_requests),
        on_quota: Some(Box::new({
            let stopper = stopper.clone();
            move || request_worker_exit(WORKER_EXIT_RECYCLE, &stopper)
        })),
        on_unhealthy: Some(Box::new({
            let stopper = stopper.clone();
            move || request_worker_exit(WORKER_EXIT_UNHEALTHY, &stopper)
        })),
        slot: env.slot_view,
    };

    let rapira = match Rapira::start_worker(mode, entrypoint, hooks, classes) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(target: "rapira", "worker PHP boot failed: {e:#}");
            return WORKER_EXIT_UNHEALTHY;
        }
    };

    rapira_master::spawn_lifeline_watch(env.lifeline);

    let name: &str = plugin.name();
    let outcome: anyhow::Result<()> =
        serve_plugin(plugin, rapira.sink(), grace, drain_grace, &stopper);
    if let Err(e) = &outcome {
        tracing::error!(target: "rapira", "plugin {name}: {e:#}");
    }
    drop(rapira);

    match WORKER_EXIT.load(SeqCst) {
        -1 if outcome.is_err() => 1,
        -1 => 0,
        code => code,
    }
}

/// Runs the plugin until it stops and joins it.
fn serve_plugin(
    plugin: Box<dyn Plugin>,
    sink: rapira_sapi::work::Sink,
    grace: Duration,
    drain_grace: Duration,
    stopper: &OnceLock<Stopper>,
) -> anyhow::Result<()> {
    let running = run_plugin(plugin, sink, grace, drain_grace)?;
    let _ = stopper.set(running.stopper());
    if WORKER_EXIT.load(SeqCst) != -1 {
        running.stop();
    }
    spawn_signal_thread(running.stopper());
    running.join()
}

/// Requires the fork bracket to have masked exactly {QUIT, INT} in the child: the first signal drains, a second force-exits 131.
fn spawn_signal_thread(stopper: Stopper) {
    std::thread::Builder::new()
        .name("rapira-worker-signal".into())
        .spawn(move || {
            let sig = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
            tracing::info!(target: "rapira", "signal {sig} received; draining worker");
            stopper.stop();
            let _ = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
            tracing::warn!(target: "rapira", "second signal; forcing worker exit");
            std::process::exit(131);
        })
        .expect("spawn worker signal thread");
}

fn sigset(signals: &[libc::c_int]) -> libc::sigset_t {
    // SAFETY: operates on a stack-owned, freshly-initialized signal set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &sig in signals {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// Blocks until one of `signals` (already blocked) is delivered. https://man7.org/linux/man-pages/man3/sigwait.3.html
fn wait_signal(signals: &[libc::c_int]) -> libc::c_int {
    // SAFETY: `set` and `sig` are stack values live for the whole call.
    unsafe {
        let set = sigset(signals);
        let mut sig: libc::c_int = 0;
        libc::sigwait(&set, &mut sig);
        sig
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sigwait` dequeues a blocked, pending signal instead of running the default terminate action.
    #[test]
    fn sigwait_reaps_a_blocked_signal() {
        let set = sigset(&[libc::SIGTERM]);
        // SAFETY: SIGTERM is blocked in this thread, so `raise` leaves it pending for `sigwait` to dequeue.
        unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::raise(libc::SIGTERM);
        }
        assert_eq!(wait_signal(&[libc::SIGTERM]), libc::SIGTERM);
    }
}
