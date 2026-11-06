use anyhow::Context;
use clap::{Args, CommandFactory, Parser, Subcommand};
use rapira_config::PoolSettings;
use rapira_master::PoolConfig;
use rapira_sapi::plugin::{Mode, Plugin};
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

mod logging;

mod observability;

mod settings;

mod worker;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The class register functions of the linked plugins. MINIT calls them in this order.
static PLUGIN_REGISTERS: [unsafe extern "C" fn(); 2] = [
    rapira_http::rapira_http_register_classes,
    rapira_grpc::rapira_grpc_register_classes,
];

#[derive(Parser)]
#[command(name = "rapira", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Boot the server: start PHP, prepare the plugins, and serve requests.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Path to rapira.toml. Relative paths inside the file resolve against its directory.
    #[arg(value_name = "CONFIG")]
    config: PathBuf,
}

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

impl PoolRun {
    /// The master's settings for the pool. The observability pool runs no PHP and has no pool table: one process, no request timeout, no memory limit.
    fn config(&self) -> PoolConfig {
        match self {
            PoolRun::Php { plugin, pool } => PoolConfig {
                name: plugin.name(),
                processes: pool.processes,
                request_terminate_timeout: pool.request_terminate_timeout,
                max_memory_mb: pool.max_memory_mb,
            },
            PoolRun::Observability { .. } => PoolConfig {
                name: "observability",
                processes: 1,
                request_terminate_timeout: Duration::ZERO,
                max_memory_mb: 0,
            },
        }
    }
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

    match Cli::parse().command {
        Some(Commands::Serve(args)) => serve(args),
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
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
fn pool_run(mut plugin: Box<dyn Plugin>, pool: PoolSettings) -> anyhow::Result<PoolRun> {
    let name: &'static str = plugin.name();
    check_mode(name, plugin.modes(), pool.mode)?;
    // An address that an earlier pool bound fails here: that pool keeps its listener open.
    plugin
        .prepare()
        .with_context(|| format!("plugin {name}: prepare failed"))?;
    Ok(PoolRun::Php { plugin, pool })
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let settings: settings::Settings = settings::resolve(&args.config)?;

    logging::init(&settings.log);
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    // The observability pool goes first: observability_body gives regions[1..] to the metrics and /readyz, and the slot cap error of the master always names a PHP pool.
    let mut pools: Vec<PoolRun> = Vec::new();
    if let Some(observability) = settings.observability {
        pools.push(observability::pool_run(observability)?);
    }
    // http pool-------
    if let Some(http) = settings.http {
        let pool: PoolSettings = http.pool.clone();
        let plugin = rapira_http::Server::from_settings(http);
        pools.push(pool_run(Box::new(plugin), pool)?);
    }
    // ---------------
    //
    // grpc pool -----
    if let Some(grpc) = settings.grpc {
        let pool: PoolSettings = grpc.pool.clone();
        let plugin = rapira_grpc::Server::from_settings(grpc)?;
        pools.push(pool_run(Box::new(plugin), pool)?);
    }
    // --------------

    let pool_cfgs: Vec<PoolConfig> = pools.iter().map(PoolRun::config).collect();

    // MINIT once, after every pool bound its listeners. Every linked plugin registers its classes, whatever pools are configured.
    let module: rapira_sapi::PhpModule = rapira_sapi::boot_master(&PLUGIN_REGISTERS)?;

    // forks ------------------------------------------------------------------
    let grace: Duration = settings.supervisor.process_control_timeout;
    let drain_grace: Duration = settings.supervisor.drain_grace();
    let cfg: rapira_master::MasterConfig = rapira_master::MasterConfig {
        pools: pool_cfgs,
        process_control_timeout: settings.supervisor.process_control_timeout,
        pidfile: settings.supervisor.pidfile,
    };
    // ------------------------------------------------------------------------

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
