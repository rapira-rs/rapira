use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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
    /// Set to true once. The plugin stops accepting, drains within `drain_grace`, and returns from `serve`.
    pub stop: watch::Receiver<bool>,
    /// The bound of the plugin's drain after the stop. The root sets it below the join bound of [`run_plugin`].
    pub drain_grace: Duration,
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
    stopper: Stopper,
    grace: Duration,
}

impl Running {
    /// Sets the stop flag. The plugin drains on its own time.
    pub fn stop(&self) {
        self.stopper.stop();
    }

    pub fn stopper(&self) -> Stopper {
        self.stopper.clone()
    }

    /// Joins the plugin thread. An error from serve, a panic, or a join past `grace` after stop is an Err.
    pub fn join(mut self) -> anyhow::Result<()> {
        let thread = self.thread.take().expect("join consumes Running");
        let events = &self.stopper.events;
        let mut state = events.lock();
        while !state.ended {
            state = match state.stopped_at {
                None => events
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(at) => {
                    let Some(left) = self.grace.checked_sub(at.elapsed()) else {
                        return Err(anyhow!(
                            "{} did not stop within {:?}",
                            self.name,
                            self.grace
                        ));
                    };
                    events
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        drop(state);
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
            self.stopper.stop();
        }
    }
}

/// Sets the stop flag from a plain thread: it needs no runtime.
#[derive(Clone)]
pub struct Stopper {
    flag: watch::Sender<bool>,
    events: Arc<JoinEvents>,
}

impl Stopper {
    pub fn stop(&self) {
        self.flag.send_replace(true);
        let mut state = self.events.lock();
        state.stopped_at.get_or_insert_with(Instant::now);
        self.events.changed.notify_all();
    }
}

/// The two events that [`Running::join`] waits for.
#[derive(Default)]
struct JoinEvents {
    state: Mutex<JoinState>,
    changed: Condvar,
}

#[derive(Default)]
struct JoinState {
    /// The first stop: the grace of the join counts from here.
    stopped_at: Option<Instant>,
    /// The plugin thread ended.
    ended: bool,
}

impl JoinEvents {
    fn lock(&self) -> MutexGuard<'_, JoinState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Marks the end of the plugin thread when it drops, also on a panic.
struct Ended(Arc<JoinEvents>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.lock().ended = true;
        self.0.changed.notify_all();
    }
}

/// Builds one two-worker tokio runtime with the IO and time drivers, spawns `rapira-{name}` and runs `serve` on it.
/// `grace` bounds the join after the stop. `drain_grace` goes to [`Worker::drain_grace`].
pub fn run_plugin(
    plugin: Box<dyn Plugin>,
    sink: Sink,
    grace: Duration,
    drain_grace: Duration,
) -> anyhow::Result<Running> {
    let name = plugin.name();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name(format!("rapira-{name}-io"))
        .build()
        .map_err(|e| anyhow!("building the {name} runtime: {e}"))?;
    let (flag, stop_rx) = watch::channel(false);
    let events = Arc::new(JoinEvents::default());
    let ended = Ended(Arc::clone(&events));
    let worker = Worker {
        handle: rt.handle().clone(),
        sink,
        stop: stop_rx,
        drain_grace,
    };
    let thread = std::thread::Builder::new()
        .name(format!("rapira-{name}"))
        .spawn(move || {
            let _ended = ended;
            let result = plugin.serve(worker);
            // The runtime drops here, on a plain thread: a drop in an async context panics.
            drop(rt);
            result
        })
        .map_err(|e| anyhow!("spawning the {name} thread: {e}"))?;
    Ok(Running {
        name,
        thread: Some(thread),
        stopper: Stopper { flag, events },
        grace,
    })
}
