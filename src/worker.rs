use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering::SeqCst};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rapira_master::{WORKER_EXIT_RECYCLE, WORKER_EXIT_UNHEALTHY, WorkerEnv};
use rapira_sapi::runtime::{ExtensionRuntime, Stopper};
use rapira_sapi::work::DispatcherClasses;
use rapira_sapi::{GrpcService, Mode, Rapira, WorkerHooks};

/// First writer wins, except unhealthy upgrades a pending recycle; -1 = unset, so the extension outcomes set the exit code.
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
    /// None for a gRPC pool.
    pub http: Option<HttpArgs>,
    /// The services a gRPC pool serves; None for an http pool.
    pub services: Option<Vec<GrpcService>>,
}

/// The worker settings of an http pool.
#[derive(Clone)]
pub struct HttpArgs {
    /// Multipart limits, with a per-worker spool dir under `dir`; None outside dispatcher mode, which parses no uploads.
    pub uploads: Option<rapira_sapi::multipart::Limits>,
    /// sendFile() containment root, canonicalized per worker.
    pub sendfile_root: PathBuf,
}

/// Returns the process exit code for the master's fork bracket; never runs PHP module teardown, MSHUTDOWN stays with the master.
pub fn worker_body(env: WorkerEnv, host: ExtensionRuntime, args: PoolArgs) -> i32 {
    let PoolArgs {
        mode,
        entrypoint,
        max_requests,
        grace,
        http,
        services,
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
    let mut uploads: Option<rapira_sapi::multipart::Limits> = http.and_then(|http| {
        rapira_sapi::set_sendfile_root(http.sendfile_root);
        http.uploads
    });
    let classes: Option<DispatcherClasses> = if services.is_some() {
        Some(rapira_sapi::grpc::DISPATCHER_CLASSES)
    } else {
        matches!(mode, Mode::Dispatcher(_)).then_some(rapira_sapi::http::DISPATCHER_CLASSES)
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
        slot: Some(env.slot_view),
        on_thread_start: services.map(|services| {
            Box::new(move || rapira_sapi::grpc::set_services(services)) as Box<dyn FnOnce() + Send>
        }),
    };

    let rapira = match Rapira::start_worker(mode, hooks, classes) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(target: "rapira", "worker PHP boot failed: {e:#}");
            return WORKER_EXIT_UNHEALTHY;
        }
    };

    rapira_master::spawn_lifeline_watch(env.lifeline);

    let mut spool_dir: Option<PathBuf> = None;
    if let Some(uploads) = uploads.as_mut() {
        uploads.dir = uploads
            .dir
            .join(format!("rapira-spool-{}", std::process::id()));
        if let Err(e) = {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&uploads.dir)
        } {
            tracing::error!(
                target: "rapira",
                "creating spool dir {}: {e}",
                uploads.dir.display()
            );
            return WORKER_EXIT_UNHEALTHY;
        }
        spool_dir = Some(uploads.dir.clone());
    }
    let running: rapira_sapi::runtime::Running = host.run_with_options(
        &rapira,
        entrypoint,
        rapira_sapi::runtime::RuntimeOptions {
            uploads: Arc::new(uploads.unwrap_or_default()),
            grace,
        },
    );
    let _ = stopper.set(running.stopper());
    if WORKER_EXIT.load(SeqCst) != -1 {
        stopper.get().expect("just set").stop();
    }

    let outcomes: Vec<Result<(), String>> = running.serve_worker();
    drop(rapira);
    if let Some(dir) = &spool_dir
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::warn!(target: "rapira", "removing spool dir {}: {e}", dir.display());
    }

    match WORKER_EXIT.load(SeqCst) {
        -1 if outcomes.iter().any(|o| o.is_err()) => 1,
        -1 => 0,
        code => code,
    }
}
