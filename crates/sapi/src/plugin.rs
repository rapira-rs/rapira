use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::rapira_plugin;
use crate::work::Intake;

pub use rapira_config::Mode;

pub type ServeFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

pub trait Plugin: Send + 'static {
    /// The TOML section and the dispatcher name PHP sees.
    fn name(&self) -> &'static str;
    /// The address of the plugin record (`rapira_plugin` in rapira_sapi.h). In dispatcher mode, PHP calls receive() on the dispatcher that the record describes.
    fn dispatcher(&self) -> *const rapira_plugin;
    /// Binds the pool configuration on each PHP thread before its first entrypoint.
    fn worker_init(&self) -> Arc<dyn Fn() + Send + Sync>;
    /// Serves the pool intake on the IO runtime until the listener fails or the drain ends. A true value or a closed channel on `stop` stops the accept loop.
    fn serve(
        self: Box<Self>,
        intake: Intake,
        stop: watch::Receiver<bool>,
        drain_grace: Duration,
    ) -> ServeFuture;
}
