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

struct WakeState {
    signals: VecDeque<libc::c_int>,
    failed: bool,
    plugins_done: bool,
    obs_done: bool,
    workers_done: bool,
}

type Wake = Arc<(Mutex<WakeState>, Condvar)>;

pub fn serve(settings: Settings, signals: Signals) -> Result<()> {
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));
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

    if !rapira_sapi::zts() {
        ensure!(
            plugins.len() == 1,
            "NTS PHP supports one pool; build PHP with --enable-zts for multiple pools"
        );
        let (plugin, pool) = &mut plugins[0];
        if pool.workers > 1 {
            warn!(target: "rapira", "{}.pool.workers = {}: NTS PHP runs one worker; build PHP with --enable-zts for multiple workers", plugin.name(), pool.workers);
            pool.workers = 1;
        }
    }
    let wake: Wake = Arc::new((
        Mutex::new(WakeState {
            signals: VecDeque::new(),
            failed: false,
            plugins_done: false,
            obs_done: obs.is_none(),
            workers_done: false,
        }),
        Condvar::new(),
    ));
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

    loop {
        let sig = {
            let mut state = wake.0.lock().unwrap();
            while !state.failed && state.signals.is_empty() {
                state = wake.1.wait(state).unwrap();
            }
            if state.failed {
                break;
            }
            state.signals.pop_front().unwrap()
        };
        match sig {
            libc::SIGTERM | libc::SIGINT | libc::SIGQUIT => break,
            libc::SIGUSR1 => status(&pools),
            libc::SIGUSR2 | libc::SIGHUP => {
                info!(target: "rapira", "reloading all pools");
                engine.reload();
            }
            _ => unreachable!(),
        }
    }

    let deadline = Instant::now() + settings.server.stop_timeout;
    stop.send_replace(true);
    info!(target: "rapira", "stopping server");
    engine.begin_drain();
    let failed = loop {
        let mut state = wake.0.lock().unwrap();
        while let Some(sig) = state.signals.pop_front() {
            match sig {
                libc::SIGTERM | libc::SIGINT => {
                    drop(state);
                    rapira_sapi::exit::exit(130);
                }
                libc::SIGUSR1 if !state.failed => {
                    drop(state);
                    status(&pools);
                    state = wake.0.lock().unwrap();
                }
                _ => {}
            }
        }
        if state.plugins_done && state.obs_done && state.workers_done {
            break state.failed;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let failed = state.failed;
            drop(state);
            error!(target: "rapira", "stop deadline reached; exiting without PHP module shutdown");
            rapira_sapi::exit::exit(i32::from(failed));
        }
        let (guard, _) = wake.1.wait_timeout(state, left).unwrap();
        drop(guard);
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
