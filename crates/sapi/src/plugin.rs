use std::pin::Pin;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::rapira_plugin;
use crate::work::Intake;

pub use rapira_config::Mode;

/// Sets the IO runtime for PHP stream waits. The host calls this once before it starts the plugin tasks.
pub fn set_runtime(handle: Handle) {
    crate::stream::HANDLE
        .set(handle)
        .expect("the host sets the IO runtime once");
}

/// The pool intake and the stop settings for one plugin.
pub struct Worker {
    pub intake: Intake,
    /// A true value or a closed channel stops the accept loop.
    pub stop: watch::Receiver<bool>,
    pub drain_grace: Duration,
}

pub type ServeFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

pub trait Plugin: Send + 'static {
    /// The TOML section and the dispatcher name PHP sees.
    fn name(&self) -> &'static str;
    /// The pool modes this plugin serves. The root refuses another mode at boot.
    fn modes(&self) -> &'static [Mode];
    /// The address of the plugin record (`rapira_plugin` in rapira_sapi.h). In dispatcher mode, PHP calls receive() on the dispatcher that the record describes.
    fn dispatcher(&self) -> *const rapira_plugin;
    /// Serves on the IO runtime until the listener fails or the drain ends.
    fn serve(self: Box<Self>, worker: Worker) -> ServeFuture;
}
