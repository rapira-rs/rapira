//! The http plugin. It serves HTTP/1.1 on the configured listener, and PHP answers every request.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rapira_http_middleware::Middleware;
use rapira_net::{ListenAddr, PreparedListener};
use rapira_sapi::plugin::{Plugin, ServeFuture};
use rapira_sapi::rapira_plugin;
use rapira_sapi::work::Intake;
use tokio::sync::watch;

pub mod check;
pub mod config;
mod handler;
mod io;
pub mod multipart;
pub mod php;
pub mod request;
mod responder;
mod serve;

pub use php::rapira_http_register_classes;
pub use rapira_http_middleware as middleware;

/// A refusal before dispatch. PHP never saw the request.
#[derive(Debug)]
pub struct Rejection {
    pub status: http::StatusCode,
    pub reason: String,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status.as_u16(), self.reason)
    }
}

pub struct Config {
    pub listen: ListenAddr,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    /// The classic and worker modes build the CGI view, so they drop the field names that alias a CGI variable.
    pub superglobals: bool,
    pub write_timeout: Duration,
    pub keepalive_timeout: Duration,
    /// `[http].middleware` in config order. The first entry is outermost.
    pub middleware: Vec<Arc<dyn Middleware>>,
    /// The multipart limits and the process spool directory for dispatcher mode.
    pub uploads: Option<multipart::Limits>,
    /// The containment root of sendFile().
    pub sendfile_root: PathBuf,
}

pub struct Server {
    config: Config,
    prepared: PreparedListener,
    spool: Option<multipart::SpoolDir>,
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "http"
    }

    fn dispatcher(&self) -> *const rapira_plugin {
        &raw const php::rapira_http_plugin
    }

    fn worker_init(&self) -> Arc<dyn Fn() + Send + Sync> {
        let identity =
            rapira_sapi::HttpIdentity::new(&self.config.server_name, self.config.server_port);
        let sendfile_root = self.config.sendfile_root.clone();
        Arc::new(move || {
            identity.enter();
            php::set_sendfile_root(sendfile_root.clone());
        })
    }

    fn serve(
        self: Box<Self>,
        intake: Intake,
        stop: watch::Receiver<bool>,
        drain_grace: Duration,
    ) -> ServeFuture {
        Box::pin(serve::serve(*self, intake, stop, drain_grace))
    }
}
