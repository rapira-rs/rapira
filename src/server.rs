use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use rapira_config::{Mode, PoolSettings};
use rapira_sapi::plugin::{Plugin, Worker};
use rapira_sapi::pool::Pool;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::pidfile::PidFile;
use crate::settings::Settings;
use crate::signals::Signals;

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
    for (plugin, pool) in &plugins {
        check_mode(plugin.name(), plugin.modes(), pool.mode)?;
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
    let count = plugins
        .iter()
        .try_fold(0usize, |n, (_, pool)| n.checked_add(pool.workers))
        .context("too many PHP workers")?;
    let mut engine = rapira_sapi::Engine::start(&crate::PLUGIN_REGISTERS, count)?;
    let timed = plugins
        .iter()
        .any(|(_, pool)| !pool.request_terminate_timeout.is_zero());
    if timed {
        rapira_sapi::watchdog::reserve_wake_signal()
            .context("the request watchdog requires a free SIGRTMAX")?;
    }
    let mut pidfile = settings
        .server
        .pidfile
        .as_deref()
        .map(PidFile::write)
        .transpose()
        .context("writing pidfile")?;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(n) = settings.server.io_threads {
        builder.worker_threads(n.get());
    }
    let rt = builder
        .enable_all()
        .thread_name("rapira-io")
        .build()
        .context("starting the IO runtime")?;
    let (stop, _) = watch::channel(false);
    let grace = settings.server.drain_grace();
    let mut services = Vec::new();
    for (plugin, cfg) in plugins {
        // SAFETY: both plugins supply static dispatcher records.
        let intake = or_exit(
            unsafe { engine.start_pool(cfg, &*plugin, rt.handle()) },
            &mut pidfile,
        );
        services.push((
            plugin,
            Worker {
                intake,
                stop: stop.subscribe(),
                drain_grace: grace,
            },
        ));
    }
    let pools: Arc<[Arc<Pool>]> = engine.pools().into();
    let failed = Arc::new(AtomicBool::new(false));
    let mut tasks = JoinSet::new();
    for (plugin, worker) in services {
        let name = plugin.name();
        tasks.spawn_on(
            async move { (name, plugin.serve(worker).await) },
            rt.handle(),
        );
    }
    let stopped = stop.subscribe();
    let error_flag = Arc::clone(&failed);
    let plugins = rt.spawn(async move {
        while let Some(result) = tasks.join_next().await {
            let (name, result) = result.unwrap_or_else(|e| ("task", Err(e.into())));
            service_end(name, result, &stopped, &error_flag);
        }
    });
    let obs = obs.map(|server| {
        let build = rapira_observability::Build {
            version: env!("CARGO_PKG_VERSION"),
            php_version: rapira_sapi::linked_php_version(),
        };
        let stopped = stop.subscribe();
        let flag = Arc::clone(&failed);
        let pools = Arc::clone(&pools);
        or_exit(
            thread::Builder::new()
                .name("rapira-obs".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        server.serve(pools, build, stopped.clone(), grace)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("observability thread panicked")));
                    service_end("observability", result, &stopped, &flag);
                })
                .context("starting observability"),
            &mut pidfile,
        )
    });
    let watchdog = timed.then(|| {
        or_exit(
            rapira_sapi::watchdog::start().context("starting the watchdog"),
            &mut pidfile,
        )
    });

    loop {
        let sig = or_exit(
            signals.wait().context("waiting for a host signal"),
            &mut pidfile,
        );
        if failed.load(Ordering::Acquire) {
            break;
        }
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
    info!(target: "rapira", "stopping server");
    engine.begin_drain();
    stop.send_replace(true);
    drop(watchdog);
    loop {
        match or_exit(
            signals.pending().context("checking signals during stop"),
            &mut pidfile,
        ) {
            Some(libc::SIGTERM | libc::SIGINT) => exit(130, &mut pidfile),
            Some(libc::SIGUSR1) => status(&pools),
            _ => {}
        }
        if plugins.is_finished()
            && obs.as_ref().is_none_or(thread::JoinHandle::is_finished)
            && engine.workers_finished()
        {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            error!(target: "rapira", "stop deadline reached; exiting without PHP module shutdown");
            exit(i32::from(failed.load(Ordering::Acquire)), &mut pidfile);
        }
        thread::sleep(left.min(Duration::from_millis(10)));
    }
    if let Some(thread) = obs {
        thread.join().expect("observability caught its panic");
    }
    drop(engine);
    rt.shutdown_background();
    ensure!(!failed.load(Ordering::Acquire), "a server listener stopped");
    Ok(())
}

/// A listener failure wakes the signal loop. During an operator stop, a cut drain still exits successfully.
fn service_end(name: &str, result: Result<()>, stop: &watch::Receiver<bool>, failed: &AtomicBool) {
    if let Err(e) = result {
        error!(target: "rapira", "plugin {name}: {e:#}");
    }
    if !*stop.borrow() {
        failed.store(true, Ordering::Release);
        // SAFETY: all threads block SIGUSR1. The main thread accepts it and checks the failure flag.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGUSR1);
        }
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

fn or_exit<T>(result: Result<T>, pidfile: &mut Option<PidFile>) -> T {
    result.unwrap_or_else(|e| {
        error!(target: "rapira", "{e:#}");
        exit(1, pidfile);
    })
}

fn exit(code: i32, pidfile: &mut Option<PidFile>) -> ! {
    drop(pidfile.take());
    // SAFETY: threads can still be inside PHP. Skip process destructors.
    unsafe { libc::_exit(code) }
}

fn check_mode(name: &str, served: &[Mode], mode: Mode) -> Result<()> {
    if served.contains(&mode) {
        return Ok(());
    }
    let served: Vec<String> = served.iter().map(Mode::to_string).collect();
    anyhow::bail!(
        "{name}.pool.mode = {mode}: this plugin serves {}",
        served.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_mode_the_plugin_does_not_serve_fails_the_boot() {
        let err = check_mode("grpc", &[Mode::Dispatcher], Mode::Worker).unwrap_err();
        assert_eq!(
            err.to_string(),
            "grpc.pool.mode = worker: this plugin serves dispatcher"
        );
    }
}
