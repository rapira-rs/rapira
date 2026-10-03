use anyhow::Context;
use rapira_config::PoolSettings;
use rapira_master::PoolConfig;
use rapira_sapi::plugin::{Mode, Plugin};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use tracing::info;

mod logging;

mod observability;

mod settings;

mod worker;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "usage: rapira serve <CONFIG> | rapira --version | rapira --help";

/// One pool's fork-time payload. The master hands out `WorkerEnv::pool` as the index into the list.
enum PoolRun {
    /// A PHP pool.
    Php {
        plugin: Box<dyn Plugin>,
        pool: PoolSettings,
    },
    /// The observability pool.
    Observability {
        server: rapira_observability::Server,
    },
}

/// Signals are blocked first: USR1/USR2/HUP terminate by default until the master installs its handlers.
fn main() -> anyhow::Result<()> {
    rapira_master::block_early_signals();
    // Transparent huge pages put the allocator regions on 2 MiB pages and increase the memory use of each worker. The call runs before PHP MINIT, and the forked workers inherit the setting. https://man7.org/linux/man-pages/man2/PR_SET_THP_DISABLE.2const.html
    #[cfg(target_os = "linux")]
    // SAFETY: prctl with integer arguments only; the kernel reads all four as unsigned long.
    unsafe {
        libc::prctl(
            libc::PR_SET_THP_DISABLE,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [cmd, config] if cmd == "serve" => serve(Path::new(config)),
        [flag] if flag == "--version" || flag == "-V" => {
            println!("rapira {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    }
}

/// `name` is the config table of the pool, for the error text. `served` is [`Plugin::modes`].
fn check_mode(name: &str, served: &[Mode], mode: Mode) -> anyhow::Result<()> {
    if served.contains(&mode) {
        return Ok(());
    }
    let served: Vec<String> = served.iter().map(Mode::to_string).collect();
    anyhow::bail!(
        "{name}.pool.mode = {mode}: this plugin serves {}",
        served.join(", ")
    )
}

/// Checks the pool mode, binds the pool's listeners and packs what the master forks with.
fn pool_run(
    mut plugin: Box<dyn Plugin>,
    pool: PoolSettings,
) -> anyhow::Result<(PoolRun, PoolConfig)> {
    let name: &'static str = plugin.name();
    check_mode(name, plugin.modes(), pool.mode)?;
    // An address that an earlier pool bound fails here: that pool keeps its listener open.
    plugin
        .prepare()
        .with_context(|| format!("plugin {name}: prepare failed"))?;
    let cfg = PoolConfig {
        name,
        processes: pool.processes,
        request_terminate_timeout: pool.request_terminate_timeout,
    };
    Ok((PoolRun::Php { plugin, pool }, cfg))
}

fn serve(config: &Path) -> anyhow::Result<()> {
    let settings: settings::Settings = settings::resolve(config)?;

    logging::init(&settings.log);
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    // `WorkerEnv::pool` indexes both lists, so they keep one order. The observability pool goes first, so the slot cap error of the master always names a PHP pool.
    let mut runs: Vec<(PoolRun, PoolConfig)> = Vec::new();
    if let Some(observability) = settings.observability {
        runs.push(observability::pool_run(observability)?);
    }
    if let Some(http) = settings.http {
        let pool: PoolSettings = http.pool.clone();
        let plugin = rapira_http::Server::from_settings(http);
        runs.push(pool_run(Box::new(plugin), pool)?);
    }
    if let Some(grpc) = settings.grpc {
        let pool: PoolSettings = grpc.pool.clone();
        let plugin = rapira_grpc::Server::from_settings(grpc)?;
        runs.push(pool_run(Box::new(plugin), pool)?);
    }
    let (mut pools, pool_cfgs): (Vec<PoolRun>, Vec<PoolConfig>) = runs.into_iter().unzip();

    // MINIT once, after every pool bound its listeners. Every linked plugin registers its classes, whatever pools are configured.
    let module: rapira_sapi::PhpModule =
        rapira_sapi::boot_master(&[rapira_http::PHP_PART, rapira_grpc::PHP_PART])?;

    // forks ------------------------------------------------------------------
    let grace: Duration = settings.supervisor.process_control_timeout;
    let drain_grace: Duration = settings.supervisor.drain_grace();
    let cfg: rapira_master::MasterConfig = rapira_master::MasterConfig {
        pools: pool_cfgs,
        process_control_timeout: settings.supervisor.process_control_timeout,
        pidfile: settings.supervisor.pidfile,
    };

    let stop: Result<rapira_master::StopReason, anyhow::Error> =
        rapira_master::run(cfg, move |env: rapira_master::WorkerEnv| {
            // The child keeps its own pool's entry and drops the others, so an orphaned child holds no other pool's listener.
            let run: PoolRun = pools.swap_remove(env.pool);
            pools.clear();
            match run {
                PoolRun::Php { plugin, pool } => {
                    worker::worker_body(env, plugin, pool, grace, drain_grace)
                }
                PoolRun::Observability { server } => {
                    observability::observability_body(env, server, drain_grace)
                }
            }
        });

    match stop {
        Ok(rapira_master::StopReason::Drained) => {
            drop(module);
            Ok(())
        }
        Ok(rapira_master::StopReason::Forced) => std::process::exit(130),
        Err(e) => {
            tracing::error!(target: "rapira", "master failed: {e:#}");
            std::process::exit(rapira_master::MASTER_EXIT_FAILBOOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::check_mode;
    use rapira_sapi::plugin::Mode;

    #[test]
    fn a_pool_mode_the_plugin_does_not_serve_fails_the_boot() {
        let err = check_mode("grpc", &[Mode::Dispatcher], Mode::Worker).unwrap_err();
        assert_eq!(
            err.to_string(),
            "grpc.pool.mode = worker: this plugin serves dispatcher"
        );
    }
}
