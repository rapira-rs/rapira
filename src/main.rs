use clap::{Args, CommandFactory, Parser, Subcommand};
use extension_api::{ListenAddr, Middleware, PrepareCtx};
use php_sys::{Mode, Rapira};
use rapira_config::{
    HttpSettings, Listen, MiddlewareSettings, RunMode, Scaling, Settings, SupervisorSettings,
    UnsafeFieldNames,
};
use rapira_http::{
    Config as HttpConfig, Server as HttpServer, UnsafeFieldNames as HttpUnsafeFieldNames,
};
use rapira_master::PoolConfig;
use rapira_runtime::ExtensionRuntime;
use rapira_scoreboard::Scoreboard;
use std::{
    fs::{OpenOptions, read_dir, remove_file},
    os::fd::RawFd,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::info;

mod logging;

mod worker;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// PHP application server driven by native extensions.
#[derive(Parser)]
#[command(name = "rapira", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Boot the server: start PHP, register extensions, and serve requests.
    Serve(ServeArgs),
    #[cfg(feature = "otel")]
    #[command(hide = true)]
    Otel,
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
    host: Option<ExtensionRuntime>,
    args: worker::PoolArgs,
}

/// Signals are blocked first: USR1/USR2/HUP terminate by default until the master installs its handlers.
fn main() -> anyhow::Result<()> {
    rapira_master::block_early_signals();

    match Cli::parse().command {
        Some(Commands::Serve(args)) => serve(args),
        #[cfg(feature = "otel")]
        Some(Commands::Otel) => {
            let config = otel::process::Config::from_env()?;
            logging::init(&config.log, None)?;
            otel::exporter::run_process(config.otel, config.max_connections)
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// kill(pid, 0) probes existence without signaling: ESRCH means the owner is gone, EPERM means it runs under another uid. https://man7.org/linux/man-pages/man2/kill.2.html
fn spool_dir_reclaimable(name: &str) -> bool {
    let Some(pid) = name
        .strip_prefix("rapira-spool-")
        .and_then(|p| p.parse::<i32>().ok())
        .filter(|&p| p > 0)
    else {
        return false;
    };
    let gone = unsafe { libc::kill(pid, 0) } == -1;
    gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Dispatcher mode only: the host spools file parts here, so the dir must exist and accept a new file. The sweep reclaims the spool dirs of masters that are gone.
fn prepare_uploads_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("creating http.uploads.dir {}: {e}", dir.display()))?;
    let probe = dir.join(format!(".rapira-probe-{}", std::process::id()));
    let _ = remove_file(&probe);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| anyhow::anyhow!("http.uploads.dir {} is not writable: {e}", dir.display()))?;
    let _ = remove_file(&probe);
    match read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if !spool_dir_reclaimable(&entry.file_name().to_string_lossy()) {
                    continue;
                }
                let path = entry.path();
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(target: "rapira", "sweeping spool dir {}: {e}", path.display());
                }
            }
        }
        Err(e) => {
            tracing::warn!(target: "rapira", "listing {} for the spool sweep: {e}", dir.display());
        }
    }
    Ok(())
}

/// Returns the listeners this host bound. One context serves every pool, so it rejects an address two pools share; it only appends, so the tail of its fd list is this host's slice.
fn prepare_pool(
    host: &mut ExtensionRuntime,
    prepare: &mut PrepareCtx,
) -> anyhow::Result<Vec<RawFd>> {
    let before: usize = prepare.listener_fds().len();
    host.prepare_all(prepare)?;
    let mut fds: Vec<RawFd> = prepare.listener_fds();
    Ok(fds.split_off(before))
}

/// Builds the http pool: its extension host, its listeners, and the supervision config the master needs.
fn http_pool(
    http: HttpSettings,
    supervisor: &SupervisorSettings,
    prepare: &mut PrepareCtx,
) -> anyhow::Result<(PoolRun, PoolConfig)> {
    let entrypoint: PathBuf = http.pool.entrypoint;
    let mode: Mode = match http.pool.mode {
        RunMode::Classic => Mode::Classic,
        RunMode::Worker => Mode::Worker(entrypoint.clone()),
        RunMode::Dispatcher => Mode::Dispatcher(entrypoint.clone()),
    };
    let dispatcher: bool = matches!(mode, Mode::Dispatcher(_));

    // middleware ---------------------------------------------------
    let mut middleware: Vec<Arc<dyn Middleware>> = Vec::new();
    for mw in http.middleware {
        match mw {
            MiddlewareSettings::Static(st) => {
                // is_dir() folds every stat error into false; metadata keeps the errno visible.
                let meta = std::fs::metadata(&st.root).map_err(|e| {
                    anyhow::anyhow!(
                        "http.static.root {} is not accessible: {e}",
                        st.root.display()
                    )
                })?;
                anyhow::ensure!(
                    meta.is_dir(),
                    "http.static.root {} is not a directory",
                    st.root.display()
                );
                // Serving needs search permission, not read; resolving `.` inside the root proves it.
                std::fs::metadata(st.root.join(".")).map_err(|e| {
                    anyhow::anyhow!(
                        "http.static.root {} is not accessible: {e}",
                        st.root.display()
                    )
                })?;
                info!(target: "rapira", "static files from {}, forbid {:?}", st.root.display(), st.forbid);
                middleware.push(Arc::new(rapira_static_files::StaticFiles::new(
                    st.root, st.forbid,
                )));
            }
        }
    }
    //----------------------------------------------------------------

    // sendFile() root: a boot-time diagnostic so a bad path is caught before the first request,
    // even under ondemand scaling where no worker forks at boot.
    if let Err(e) = std::fs::metadata(&http.sendfile_root) {
        tracing::warn!(
            target: "rapira",
            "http.sendfile.root {} is not accessible: {e}; sendFile() will reject every path",
            http.sendfile_root.display()
        );
    }

    // parse HTTP configuration -------------------------------------
    let http_cfg: HttpConfig = HttpConfig {
        listen: match http.listen {
            Listen::Tcp(addr) => ListenAddr::Tcp(addr),
            Listen::Unix(path) => ListenAddr::Unix(path),
        },
        server_name: http.server_name,
        server_port: http.server_port,
        max_body_size: http.max_body_size,
        write_timeout: http.write_timeout,
        drain_grace: supervisor.drain_grace(),
        unsafe_field_names: match http.unsafe_field_names {
            UnsafeFieldNames::Drop => HttpUnsafeFieldNames::Drop,
            UnsafeFieldNames::Reject => HttpUnsafeFieldNames::Reject,
        },
        superglobals: !dispatcher,
        keepalive_timeout: http.keepalive_timeout,
        middleware,
    };
    //----------------------------------------------------------------

    // uploads -------------------------------------------------------
    if dispatcher {
        prepare_uploads_dir(&http.uploads.dir)?;
    }
    let uploads = rapira_runtime::multipart::Limits {
        dir: http.uploads.dir,
        max_file_size: http.uploads.max_file_size,
        max_field_size: http.uploads.max_field_size,
        max_files: http.uploads.max_files,
        max_parts: http.uploads.max_parts,
        max_part_headers: http.uploads.max_part_headers,
    };
    // ---------------------------------------------------------------------

    let mut host: ExtensionRuntime = ExtensionRuntime::new();
    host.register::<HttpServer>(http_cfg)?;
    let listeners: Vec<RawFd> = prepare_pool(&mut host, prepare)?;

    Ok((
        PoolRun {
            host: Some(host),
            args: worker::PoolArgs {
                mode,
                entrypoint,
                max_requests: http.pool.max_requests,
                uploads,
                sendfile_root: http.sendfile_root,
                grace: supervisor.process_control_timeout,
            },
        },
        PoolConfig {
            name: "http",
            processes: http.pool.processes,
            scaling: match http.pool.scaling {
                Scaling::Static => rapira_master::Scaling::Static,
                Scaling::Dynamic {
                    min_spare,
                    max_spare,
                } => rapira_master::Scaling::Dynamic {
                    min_spare,
                    max_spare,
                },
                Scaling::Ondemand => rapira_master::Scaling::Ondemand,
            },
            process_idle_timeout: http.pool.process_idle_timeout,
            request_terminate_timeout: http.pool.request_terminate_timeout,
            listeners,
        },
    ))
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let settings: Settings = rapira_config::resolve(&args.config)?;

    #[cfg(not(feature = "otel"))]
    anyhow::ensure!(
        !settings.otel.enabled,
        "otel.enabled requires a build with the otel feature"
    );
    #[cfg(feature = "otel")]
    let exporter = settings
        .otel
        .enabled
        .then(|| {
            otel::process::Process::prepare(otel::process::Config {
                otel: settings.otel.clone(),
                log: settings.log.clone(),
                max_connections: settings
                    .http
                    .pool
                    .processes
                    .saturating_mul(2)
                    .saturating_add(1),
            })
        })
        .transpose()?;
    logging::init(
        &settings.log,
        #[cfg(feature = "otel")]
        exporter
            .as_ref()
            .map(|process| (&settings.otel, process.sender())),
    )?;
    #[cfg(feature = "otel")]
    let parent_fds = exporter
        .as_ref()
        .map_or_else(Vec::new, |process| process.parent_fds());
    #[cfg(feature = "otel")]
    let service: Box<dyn rapira_master::Service> = match exporter {
        Some(process) => Box::new(OtelService(process)),
        None => Box::new(()),
    };
    #[cfg(not(feature = "otel"))]
    let service: Box<dyn rapira_master::Service> = Box::new(());
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    // One context for every pool, kept alive past `run` so the master keeps its listener dups.
    let mut prepare: PrepareCtx = PrepareCtx::new();
    let (mut pools, pool_cfgs): (Vec<PoolRun>, Vec<PoolConfig>) =
        [tracing::trace_span!("http.prepare")
            .in_scope(|| http_pool(settings.http, &settings.supervisor, &mut prepare))?]
        .into_iter()
        .unzip();

    // MINIT once, after every pool bound its listeners.
    let module: php_sys::PhpModule =
        tracing::trace_span!("php.module.init").in_scope(Rapira::boot_master)?;

    // forks ------------------------------------------------------------------
    let cfg: rapira_master::MasterConfig = rapira_master::MasterConfig {
        pools: pool_cfgs,
        process_control_timeout: settings.supervisor.process_control_timeout,
        pidfile: settings.supervisor.pidfile,
    };
    let scoreboard: Scoreboard = Scoreboard::create(cfg.scoreboard_slots()?)?;
    let pool_names: Vec<_> = cfg.pools.iter().map(|pool| pool.name).collect();

    let stop: Result<rapira_master::StopReason, anyhow::Error> = rapira_master::run_with_service(
        cfg,
        scoreboard,
        move |env: rapira_master::WorkerEnv| {
            #[cfg(feature = "otel")]
            for &fd in &parent_fds {
                // SAFETY: the child releases its copies of master-only service descriptors.
                unsafe {
                    libc::close(fd);
                }
            }
            otel::after_fork(pool_names[env.pool]);
            let pool: &mut PoolRun = &mut pools[env.pool];
            let host: ExtensionRuntime = pool.host.take().expect("fresh child owns the host copy");
            worker::worker_body(env, host, pool.args.clone())
        },
        service,
    );

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

#[cfg(feature = "otel")]
struct OtelService(otel::process::Process);

#[cfg(feature = "otel")]
impl rapira_master::Service for OtelService {
    fn tick(&mut self) {
        self.0.tick();
    }
    fn on_exit(&mut self, pid: libc::pid_t, status: libc::c_int) -> bool {
        self.0.on_exit(pid, status)
    }
}

#[cfg(test)]
mod tests {
    use super::{prepare_pool, spool_dir_reclaimable};
    use extension_api::{ListenAddr, PrepareCtx};
    use rapira_http::{Config as HttpConfig, Server as HttpServer};
    use rapira_runtime::ExtensionRuntime;

    fn ephemeral_host() -> ExtensionRuntime {
        let mut host = ExtensionRuntime::new();
        host.register::<HttpServer>(HttpConfig {
            listen: ListenAddr::Tcp("127.0.0.1:0".parse().expect("loopback addr")),
            ..HttpConfig::default()
        })
        .expect("register the http extension");
        host
    }

    /// Two hosts bind on one shared context; each call reports only the fd its own host bound.
    #[test]
    fn prepare_pool_returns_only_the_fds_its_host_bound() {
        let mut prepare = PrepareCtx::new();
        let mut first = ephemeral_host();
        let mut second = ephemeral_host();

        let first_fds = prepare_pool(&mut first, &mut prepare).expect("prepare the first pool");
        let second_fds = prepare_pool(&mut second, &mut prepare).expect("prepare the second pool");

        assert_eq!(first_fds.len(), 1, "{first_fds:?}");
        assert_eq!(second_fds.len(), 1, "{second_fds:?}");
        assert_ne!(first_fds[0], second_fds[0]);
        assert_eq!(prepare.listener_fds().len(), 2);
    }

    /// The sweep reclaims only dirs whose owning process is gone.
    #[test]
    fn spool_sweep_reclaims_only_dead_pid_dirs() {
        assert!(!spool_dir_reclaimable("other-dir"));
        assert!(!spool_dir_reclaimable("rapira-spool-"));
        assert!(!spool_dir_reclaimable("rapira-spool-x"));
        assert!(!spool_dir_reclaimable("rapira-spool--5"));
        assert!(!spool_dir_reclaimable("rapira-spool-0"));
        let live = std::process::id();
        assert!(!spool_dir_reclaimable(&format!("rapira-spool-{live}")));

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived child");
        let dead = child.id();
        let _ = child.wait();
        assert!(spool_dir_reclaimable(&format!("rapira-spool-{dead}")));
    }
}
