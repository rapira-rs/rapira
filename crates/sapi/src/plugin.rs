use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use tokio::sync::watch;

use crate::work::{DispatcherClasses, Sink};

pub use rapira_config::Mode;

/// One plugin's PHP surface.
#[derive(Clone, Copy)]
pub struct PhpPart {
    /// Registers the plugin's classes. Runs in MINIT after the base classes.
    pub register: unsafe extern "C" fn(),
    pub dispatcher: Option<DispatcherClasses>,
}

/// What the worker hands to [`Plugin::serve`].
pub struct Worker {
    pub handle: tokio::runtime::Handle,
    pub sink: Sink,
    /// Set to true once. The plugin stops accepting, drains within `grace`, and returns from `serve`.
    pub stop: watch::Receiver<bool>,
    pub grace: Duration,
    pub entrypoint: PathBuf,
    pub mode: Mode,
}

pub trait Plugin: Send + 'static {
    /// The TOML section and the dispatcher name PHP sees.
    fn name(&self) -> &'static str;
    /// The pool modes this plugin serves. The root refuses another mode at boot.
    fn modes(&self) -> &'static [Mode];
    /// The plugin's PHP surface. None for a plugin without one.
    fn php(&self) -> Option<PhpPart>;
    /// Master side, before the fork, no runtime.
    fn prepare(&mut self, ctx: &mut rapira_net::PrepareCtx) -> anyhow::Result<()>;
    /// Worker side, on the plugin thread. Returns after the stop signal and the drain.
    fn serve(self: Box<Self>, worker: Worker) -> anyhow::Result<()>;
}

/// The plugin thread that [`run_plugin`] started.
pub struct Running {
    name: &'static str,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    stop: watch::Sender<bool>,
    grace: Duration,
}

impl Running {
    /// Sets the stop flag. The plugin drains on its own time.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    pub fn stopper(&self) -> Stopper {
        Stopper(self.stop.clone())
    }

    /// Joins the plugin thread. An error from serve, a panic, or a join past `grace` after stop is an Err.
    pub fn join(mut self) -> anyhow::Result<()> {
        let thread = self.thread.take().expect("join consumes Running");
        let mut stopped_at: Option<Instant> = None;
        while !thread.is_finished() {
            if stopped_at.is_none() && *self.stop.borrow() {
                stopped_at = Some(Instant::now());
            }
            if stopped_at.is_some_and(|at| at.elapsed() > self.grace) {
                return Err(anyhow!(
                    "{} did not stop within {:?}",
                    self.name,
                    self.grace
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        thread.join().map_err(|payload| {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            anyhow!("{} panicked: {msg}", self.name)
        })?
    }
}

impl Drop for Running {
    /// An unjoined plugin thread gets the stop flag and ends on its own.
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop.send_replace(true);
        }
    }
}

/// Sets the stop flag from a plain thread: it needs no runtime.
#[derive(Clone)]
pub struct Stopper(watch::Sender<bool>);

impl Stopper {
    pub fn stop(&self) {
        self.0.send_replace(true);
    }
}

/// Builds one two-worker tokio runtime with the IO and time drivers, spawns `rapira-{name}` and runs `serve` on it.
pub fn run_plugin(
    plugin: Box<dyn Plugin>,
    sink: Sink,
    grace: Duration,
    entrypoint: PathBuf,
    mode: Mode,
) -> anyhow::Result<Running> {
    let name = plugin.name();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name(format!("rapira-{name}-io"))
        .build()
        .map_err(|e| anyhow!("building the {name} runtime: {e}"))?;
    let (stop, stop_rx) = watch::channel(false);
    let worker = Worker {
        handle: rt.handle().clone(),
        sink,
        stop: stop_rx,
        grace,
        entrypoint,
        mode,
    };
    let thread = std::thread::Builder::new()
        .name(format!("rapira-{name}"))
        .spawn(move || {
            let result = plugin.serve(worker);
            // The runtime drops here, on a plain thread: a drop in an async context panics.
            drop(rt);
            result
        })
        .map_err(|e| anyhow!("spawning the {name} thread: {e}"))?;
    Ok(Running {
        name,
        thread: Some(thread),
        stop,
        grace,
    })
}
