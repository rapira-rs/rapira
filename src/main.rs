use clap::{Args, CommandFactory, Parser, Subcommand};
use rapira_config::{PoolSettings, SupervisorSettings};
use rapira_master::PoolConfig;
use rapira_net::PrepareCtx;
use rapira_sapi::plugin::{Mode, Plugin};
use std::{os::fd::RawFd, path::PathBuf};
use tracing::info;

mod logging;

mod settings;

mod worker;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
struct PoolRun {
    /// Taken exactly once, in the forked child.
    plugin: Option<Box<dyn Plugin>>,
    args: worker::PoolArgs,
}

/// Signals are blocked first: USR1/USR2/HUP terminate by default until the master installs its handlers.
fn main() -> anyhow::Result<()> {
    rapira_master::block_early_signals();

    match Cli::parse().command {
        Some(Commands::Serve(args)) => serve(args),
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// Returns the listeners this plugin bound. One context serves every pool, so it rejects an address two pools share; it only appends, so the tail of its fd list is this plugin's slice.
fn prepare_pool(plugin: &mut dyn Plugin, prepare: &mut PrepareCtx) -> anyhow::Result<Vec<RawFd>> {
    use anyhow::Context;
    let before: usize = prepare.listener_fds().len();
    plugin
        .prepare(prepare)
        .with_context(|| format!("plugin {}: prepare failed", plugin.name()))?;
    let mut fds: Vec<RawFd> = prepare.listener_fds();
    Ok(fds.split_off(before))
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

/// The supervision config the master needs for one pool.
fn pool_config(name: &'static str, pool: &PoolSettings, listeners: Vec<RawFd>) -> PoolConfig {
    PoolConfig {
        name,
        processes: pool.processes,
        scaling: pool.scaling,
        process_idle_timeout: pool.process_idle_timeout,
        request_terminate_timeout: pool.request_terminate_timeout,
        listeners,
    }
}

/// Checks the pool mode, binds the pool's listeners and packs what the master forks with.
fn pool_run(
    mut plugin: Box<dyn Plugin>,
    pool: &PoolSettings,
    prepare: &mut PrepareCtx,
    supervisor: &SupervisorSettings,
) -> anyhow::Result<(PoolRun, PoolConfig)> {
    let name: &'static str = plugin.name();
    check_mode(name, plugin.modes(), pool.mode)?;
    let listeners: Vec<RawFd> = prepare_pool(plugin.as_mut(), prepare)?;
    Ok((
        PoolRun {
            plugin: Some(plugin),
            args: worker::PoolArgs {
                mode: pool.mode,
                entrypoint: pool.entrypoint.clone(),
                max_requests: pool.max_requests,
                grace: supervisor.process_control_timeout,
                drain_grace: supervisor.drain_grace(),
            },
        },
        pool_config(name, pool, listeners),
    ))
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let settings: settings::Settings = settings::resolve(&args.config)?;

    logging::init(&settings.log);
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    // One plugin per configured table, with its pool.
    let mut plugins: Vec<(Box<dyn Plugin>, PoolSettings)> = Vec::new();
    if let Some(http) = settings.http {
        let pool: PoolSettings = http.pool.clone();
        let plugin = rapira_http::Server::from_settings(http);
        plugins.push((Box::new(plugin), pool));
    }
    if let Some(grpc) = settings.grpc {
        let pool: PoolSettings = grpc.pool.clone();
        let plugin = rapira_grpc::Server::from_settings(grpc)?;
        plugins.push((Box::new(plugin), pool));
    }

    // One context for every pool, kept alive past `run` so the master keeps its listener dups.
    let mut prepare: PrepareCtx = PrepareCtx::new();
    // `WorkerEnv::pool` indexes both lists, so they keep one order.
    let (mut pools, pool_cfgs): (Vec<PoolRun>, Vec<PoolConfig>) = plugins
        .into_iter()
        .map(|(plugin, pool)| pool_run(plugin, &pool, &mut prepare, &settings.supervisor))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .unzip();

    // MINIT once, after every pool bound its listeners. Every linked plugin registers its classes, whatever pools are configured.
    let module: rapira_sapi::PhpModule =
        rapira_sapi::boot_master(&[rapira_http::PHP_PART, rapira_grpc::PHP_PART])?;

    // forks ------------------------------------------------------------------
    let cfg: rapira_master::MasterConfig = rapira_master::MasterConfig {
        pools: pool_cfgs,
        process_control_timeout: settings.supervisor.process_control_timeout,
        pidfile: settings.supervisor.pidfile,
    };

    let stop: Result<rapira_master::StopReason, anyhow::Error> =
        rapira_master::run(cfg, move |env: rapira_master::WorkerEnv| {
            let pool: &mut PoolRun = &mut pools[env.pool];
            let plugin: Box<dyn Plugin> = pool
                .plugin
                .take()
                .expect("fresh child owns the plugin copy");
            worker::worker_body(env, plugin, pool.args.clone())
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
