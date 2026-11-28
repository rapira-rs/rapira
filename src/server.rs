use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rapira_config::PoolSettings;
use rapira_sapi::plugin::Plugin;
use rapira_sapi::pool::Pool;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::pidfile::PidFile;
use crate::settings::Settings;
use crate::signals::Signals;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Draining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Wait,
    PrintStatus,
    Reload,
    BeginDrain,
    Finish { failed: bool },
    ForceExit(i32),
}

struct Supervisor {
    phase: Phase,
    signals: VecDeque<libc::c_int>,
    failed: bool,
    plugins_done: bool,
    obs_done: bool,
    workers_done: bool,
}

impl Supervisor {
    fn new(has_observability: bool) -> Self {
        Self {
            phase: Phase::Running,
            signals: VecDeque::new(),
            failed: false,
            plugins_done: false,
            obs_done: !has_observability,
            workers_done: false,
        }
    }

    fn next_action(&mut self) -> Action {
        match self.phase {
            Phase::Running => {
                if self.failed {
                    self.phase = Phase::Draining;
                    return Action::BeginDrain;
                }
                while let Some(sig) = self.signals.pop_front() {
                    match sig {
                        libc::SIGTERM | libc::SIGINT | libc::SIGQUIT => {
                            self.phase = Phase::Draining;
                            return Action::BeginDrain;
                        }
                        libc::SIGUSR1 => return Action::PrintStatus,
                        libc::SIGUSR2 | libc::SIGHUP => return Action::Reload,
                        _ => {}
                    }
                }
                Action::Wait
            }
            Phase::Draining => {
                while let Some(sig) = self.signals.pop_front() {
                    match sig {
                        libc::SIGTERM | libc::SIGINT => return Action::ForceExit(130),
                        libc::SIGUSR1 if !self.failed => return Action::PrintStatus,
                        _ => {}
                    }
                }
                if self.plugins_done && self.obs_done && self.workers_done {
                    Action::Finish {
                        failed: self.failed,
                    }
                } else {
                    Action::Wait
                }
            }
        }
    }
}

type Wake = Arc<(Mutex<Supervisor>, Condvar)>;

/// Enforces NTS single-pool and single-worker limits before any listener binds or spool dir sweeps.
fn apply_nts_fallback(settings: &mut Settings, zts: bool) -> Result<()> {
    if zts {
        return Ok(());
    }
    let mut active: Option<(&'static str, &mut PoolSettings)> = None;
    if let Some(http) = &mut settings.http {
        active = Some(("http", &mut http.pool));
    }
    if let Some(grpc) = &mut settings.grpc {
        ensure!(
            active.is_none(),
            "NTS PHP supports one pool; build PHP with --enable-zts for multiple pools"
        );
        active = Some(("grpc", &mut grpc.pool));
    }
    if let Some((name, pool)) = active
        && pool.workers > 1
    {
        warn!(
            target: "rapira",
            "{name}.pool.workers = {}: NTS PHP runs one worker; build PHP with --enable-zts for multiple workers",
            pool.workers
        );
        pool.workers = 1;
    }
    Ok(())
}

pub fn serve(mut settings: Settings, signals: Signals) -> Result<()> {
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));
    apply_nts_fallback(&mut settings, rapira_sapi::zts())?;

    let obs = settings
        .observability
        .map(rapira_observability::Server::from_settings)
        .transpose()
        .context("preparing observability")?;
    let mut plugins: Vec<(Box<dyn Plugin>, PoolSettings)> = Vec::new();
    if let Some(http) = settings.http {
        let pool = http.pool.clone();
        plugins.push((Box::new(rapira_http::Server::from_settings(http)?), pool));
    }
    if let Some(grpc) = settings.grpc {
        let pool = grpc.pool.clone();
        plugins.push((Box::new(rapira_grpc::Server::from_settings(grpc)?), pool));
    }

    let wake: Wake = Arc::new((Mutex::new(Supervisor::new(obs.is_some())), Condvar::new()));
    {
        let wake = Arc::clone(&wake);
        signals
            .start(move |sig| {
                let mut state = wake.0.lock().unwrap();
                state.signals.push_back(sig);
                wake.1.notify_one();
            })
            .context("starting signal thread")?;
    }
    let mut engine = rapira_sapi::Engine::start(
        &crate::PLUGIN_REGISTERS,
        plugins.iter().map(|(_, pool)| pool),
    )?;
    let _pidfile = settings
        .server
        .pidfile
        .as_deref()
        .map(PidFile::write)
        .transpose()
        .context("writing pidfile")?;
    let has_grpc = plugins.iter().any(|(plugin, _)| plugin.name() == "grpc");
    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
    if has_grpc {
        rt_builder.thread_name("rapira-io");
    } else {
        // HTTP runs its own sharded single-threaded IO runtimes, so the control runtime only needs one thread for accept and timer support.
        rt_builder.worker_threads(1).thread_name("rapira-ctrl");
    }
    let rt = rt_builder.build().context("starting the IO runtime")?;
    let (stop, _) = watch::channel(false);
    let grace = settings.server.drain_grace();
    let mut services = Vec::new();
    for (plugin, cfg) in plugins {
        // SAFETY: both plugins supply static dispatcher records.
        let intake = or_exit(unsafe { engine.start_pool(cfg, &*plugin, rt.handle()) });
        services.push((plugin, intake));
    }
    {
        let wake = Arc::clone(&wake);
        engine.set_workers_done_notify(Arc::new(move || {
            let mut state = wake.0.lock().unwrap();
            state.workers_done = true;
            wake.1.notify_one();
        }));
    }
    let pools: Arc<[Arc<Pool>]> = engine.pools().into();
    let mut tasks = JoinSet::new();
    for (plugin, intake) in services {
        let name = plugin.name();
        let stop = stop.subscribe();
        tasks.spawn_on(
            async move { (name, plugin.serve(intake, stop, grace).await) },
            rt.handle(),
        );
    }
    let stopped = stop.subscribe();
    let plugins_wake = Arc::clone(&wake);
    rt.spawn(async move {
        while let Some(result) = tasks.join_next().await {
            let (name, result) = result.unwrap_or_else(|e| ("task", Err(e.into())));
            service_end(name, result, &stopped, &plugins_wake);
        }
        let mut state = plugins_wake.0.lock().unwrap();
        state.plugins_done = true;
        plugins_wake.1.notify_one();
    });
    let obs = obs.map(|server| {
        let build = rapira_observability::Build {
            version: env!("CARGO_PKG_VERSION"),
            php_version: rapira_sapi::linked_php_version(),
        };
        let stopped = stop.subscribe();
        let obs_wake = Arc::clone(&wake);
        let pools = Arc::clone(&pools);
        or_exit(
            thread::Builder::new()
                .name("rapira-obs".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        server.serve(pools, build, stopped.clone(), grace)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("observability thread panicked")));
                    service_end("observability", result, &stopped, &obs_wake);
                    let mut state = obs_wake.0.lock().unwrap();
                    state.obs_done = true;
                    obs_wake.1.notify_one();
                })
                .context("starting observability"),
        )
    });

    let mut deadline: Option<Instant> = None;
    let failed = loop {
        let action = {
            let mut state = wake.0.lock().unwrap();
            loop {
                let action = state.next_action();
                if action != Action::Wait {
                    break action;
                }
                match deadline {
                    None => state = wake.1.wait(state).unwrap(),
                    Some(until) => {
                        let left = until.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            let code = i32::from(state.failed);
                            drop(state);
                            error!(target: "rapira", "stop deadline reached; exiting without PHP module shutdown");
                            rapira_sapi::exit::exit(code);
                        }
                        let (guard, _) = wake.1.wait_timeout(state, left).unwrap();
                        state = guard;
                    }
                }
            }
        };
        match action {
            Action::Wait => unreachable!(),
            Action::PrintStatus => status(&pools),
            Action::Reload => {
                info!(target: "rapira", "reloading all pools");
                engine.reload();
            }
            Action::BeginDrain => {
                deadline = Some(Instant::now() + settings.server.stop_timeout);
                stop.send_replace(true);
                info!(target: "rapira", "stopping server");
                engine.begin_drain();
            }
            Action::Finish { failed } => break failed,
            Action::ForceExit(code) => rapira_sapi::exit::exit(code),
        }
    };
    if let Some(thread) = obs {
        thread.join().expect("observability caught its panic");
    }
    drop(engine);
    rt.shutdown_background();
    ensure!(!failed, "a server listener stopped");
    Ok(())
}

/// A listener failure wakes the main loop directly. During an operator stop, a cut drain still exits successfully.
fn service_end(name: &str, result: Result<()>, stop: &watch::Receiver<bool>, wake: &Wake) {
    if let Err(e) = result {
        error!(target: "rapira", "plugin {name}: {e:#}");
    }
    if !*stop.borrow() {
        let mut state = wake.0.lock().unwrap();
        state.failed = true;
        wake.1.notify_one();
    }
}

fn status(pools: &[Arc<Pool>]) {
    for pool in pools {
        let p = pool.snapshot();
        info!(target: "rapira", "pool={} workers={} pending={} failed_on_full_queue={} refused={}", p.name, p.workers.len(), p.pending, p.failed_on_full_queue, p.refused);
        for (i, w) in p.workers.iter().enumerate() {
            info!(target: "rapira", "pool={} worker={i} state={} handled={} errors={} recycles={} last_activity_ms={}", p.name, w.state.label(), w.handled, w.errors, w.recycles, w.last_activity_ms);
        }
    }
}

fn or_exit<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|e| {
        error!(target: "rapira", "{e:#}");
        rapira_sapi::exit::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::settings::{FileConfig, settings as resolve_settings};

    #[test]
    fn nts_fallback_clamps_workers_and_refuses_two_pools() {
        struct Case {
            name: &'static str,
            toml: &'static str,
            zts: bool,
            want_err: Option<&'static str>,
            want_http_workers: Option<usize>,
            want_grpc_workers: Option<usize>,
        }
        let cases = [
            Case {
                name: "zts keeps multiple pools and configured worker counts",
                toml: "[http.pool]\nentrypoint = \"Cargo.toml\"\nworkers = 4\n\
                       [grpc]\ndescriptor_set = \"Cargo.toml\"\n\
                       [grpc.pool]\nentrypoint = \"Cargo.toml\"\nworkers = 3\n",
                zts: true,
                want_err: None,
                want_http_workers: Some(4),
                want_grpc_workers: Some(3),
            },
            Case {
                name: "nts accepts single http pool with one worker",
                toml: "[http.pool]\nentrypoint = \"Cargo.toml\"\nworkers = 1\n",
                zts: false,
                want_err: None,
                want_http_workers: Some(1),
                want_grpc_workers: None,
            },
            Case {
                name: "nts clamps single http pool above one worker to one",
                toml: "[http.pool]\nentrypoint = \"Cargo.toml\"\nworkers = 4\n",
                zts: false,
                want_err: None,
                want_http_workers: Some(1),
                want_grpc_workers: None,
            },
            Case {
                name: "nts clamps single grpc pool above one worker to one",
                toml: "[grpc]\ndescriptor_set = \"Cargo.toml\"\n\
                       [grpc.pool]\nentrypoint = \"Cargo.toml\"\nworkers = 3\n",
                zts: false,
                want_err: None,
                want_http_workers: None,
                want_grpc_workers: Some(1),
            },
            Case {
                name: "nts refuses two pools before binding listeners",
                toml: "[http.pool]\nentrypoint = \"Cargo.toml\"\n\
                       [grpc]\ndescriptor_set = \"Cargo.toml\"\n\
                       [grpc.pool]\nentrypoint = \"Cargo.toml\"\n",
                zts: false,
                want_err: Some(
                    "NTS PHP supports one pool; build PHP with --enable-zts for multiple pools",
                ),
                want_http_workers: None,
                want_grpc_workers: None,
            },
        ];

        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        for case in cases {
            let file: FileConfig = toml::from_str(case.toml).expect(case.name);
            let mut settings = resolve_settings(file, dir).expect(case.name);
            let res = apply_nts_fallback(&mut settings, case.zts);
            match case.want_err {
                Some(want) => {
                    let err = res.expect_err(case.name).to_string();
                    assert_eq!(err, want, "{}", case.name);
                }
                None => {
                    res.expect(case.name);
                    assert_eq!(
                        settings.http.as_ref().map(|h| h.pool.workers),
                        case.want_http_workers,
                        "{}",
                        case.name
                    );
                    assert_eq!(
                        settings.grpc.as_ref().map(|g| g.pool.workers),
                        case.want_grpc_workers,
                        "{}",
                        case.name
                    );
                }
            }
        }
    }

    #[test]
    fn supervisor_transitions_for_run_drain_and_failure() {
        #[derive(Clone, Copy)]
        enum Event {
            Signal(libc::c_int),
            ServiceFailed,
            PluginsDone,
            ObsDone,
            WorkersDone,
        }
        struct Case {
            name: &'static str,
            has_obs: bool,
            events: &'static [Event],
            want_actions: &'static [Action],
            want_deadline_code: i32,
        }
        let cases = [
            Case {
                name: "running status and reload signals emit actions in arrival order",
                has_obs: false,
                events: &[
                    Event::Signal(libc::SIGUSR1),
                    Event::Signal(libc::SIGHUP),
                    Event::Signal(libc::SIGUSR2),
                ],
                want_actions: &[Action::PrintStatus, Action::Reload, Action::Reload],
                want_deadline_code: 0,
            },
            Case {
                name: "sigterm drains until plugins obs and workers all finish",
                has_obs: true,
                events: &[
                    Event::Signal(libc::SIGTERM),
                    Event::PluginsDone,
                    Event::WorkersDone,
                    Event::ObsDone,
                ],
                want_actions: &[
                    Action::BeginDrain,
                    Action::Wait,
                    Action::Wait,
                    Action::Finish { failed: false },
                ],
                want_deadline_code: 0,
            },
            Case {
                name: "sigint without observability finishes once plugins and workers exit",
                has_obs: false,
                events: &[
                    Event::Signal(libc::SIGINT),
                    Event::PluginsDone,
                    Event::WorkersDone,
                ],
                want_actions: &[
                    Action::BeginDrain,
                    Action::Wait,
                    Action::Finish { failed: false },
                ],
                want_deadline_code: 0,
            },
            Case {
                name: "sigquit drains and ignores second sigquit sighup and sigusr2",
                has_obs: false,
                events: &[
                    Event::Signal(libc::SIGQUIT),
                    Event::Signal(libc::SIGQUIT),
                    Event::Signal(libc::SIGHUP),
                    Event::Signal(libc::SIGUSR2),
                    Event::PluginsDone,
                    Event::WorkersDone,
                ],
                want_actions: &[
                    Action::BeginDrain,
                    Action::Wait,
                    Action::Wait,
                    Action::Wait,
                    Action::Wait,
                    Action::Finish { failed: false },
                ],
                want_deadline_code: 0,
            },
            Case {
                name: "second sigterm during drain forces exit 130",
                has_obs: false,
                events: &[Event::Signal(libc::SIGTERM), Event::Signal(libc::SIGTERM)],
                want_actions: &[Action::BeginDrain, Action::ForceExit(130)],
                want_deadline_code: 0,
            },
            Case {
                name: "second sigint during drain forces exit 130",
                has_obs: false,
                events: &[Event::Signal(libc::SIGINT), Event::Signal(libc::SIGINT)],
                want_actions: &[Action::BeginDrain, Action::ForceExit(130)],
                want_deadline_code: 0,
            },
            Case {
                name: "sigusr1 prints status during a healthy drain",
                has_obs: false,
                events: &[
                    Event::Signal(libc::SIGTERM),
                    Event::Signal(libc::SIGUSR1),
                    Event::PluginsDone,
                    Event::WorkersDone,
                ],
                want_actions: &[
                    Action::BeginDrain,
                    Action::PrintStatus,
                    Action::Wait,
                    Action::Finish { failed: false },
                ],
                want_deadline_code: 0,
            },
            Case {
                name: "pre-stop listener failure enters drain suppresses status and exits 1",
                has_obs: true,
                events: &[
                    Event::Signal(libc::SIGUSR1),
                    Event::ServiceFailed,
                    Event::ObsDone,
                    Event::PluginsDone,
                    Event::WorkersDone,
                ],
                want_actions: &[
                    Action::PrintStatus,
                    Action::BeginDrain,
                    Action::Wait,
                    Action::Wait,
                    Action::Finish { failed: true },
                ],
                want_deadline_code: 1,
            },
            Case {
                name: "sigusr1 during a failed drain is suppressed",
                has_obs: false,
                events: &[
                    Event::ServiceFailed,
                    Event::Signal(libc::SIGUSR1),
                    Event::PluginsDone,
                    Event::WorkersDone,
                ],
                want_actions: &[
                    Action::BeginDrain,
                    Action::Wait,
                    Action::Wait,
                    Action::Finish { failed: true },
                ],
                want_deadline_code: 1,
            },
        ];

        for case in cases {
            let mut sup = Supervisor::new(case.has_obs);
            let mut got = Vec::new();
            for &event in case.events {
                match event {
                    Event::Signal(sig) => sup.signals.push_back(sig),
                    Event::ServiceFailed => sup.failed = true,
                    Event::PluginsDone => sup.plugins_done = true,
                    Event::ObsDone => sup.obs_done = true,
                    Event::WorkersDone => sup.workers_done = true,
                }
                got.push(sup.next_action());
            }
            assert_eq!(got, case.want_actions, "{}", case.name);
            assert_eq!(
                i32::from(sup.failed),
                case.want_deadline_code,
                "{}",
                case.name
            );
        }
    }
}
