//! The http plugin: HTTP/1.1 on the configured listener, every request answered from PHP.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use rapira_net::{ListenAddr, PreparedListener};
use rapira_sapi::plugin::{Mode, Plugin, Worker};
use rapira_sapi::work::DispatcherClasses;

mod bridge;
pub mod check;
pub mod config;
mod handler;
pub mod middleware;
pub mod multipart;
pub mod php;
pub mod request;
mod response;
mod serve;

pub use php::rapira_http_register_classes;

pub struct Config {
    pub listen: ListenAddr,
    pub server_name: &'static str,
    pub server_port: u16,
    pub max_body_size: usize,
    /// The classic and worker modes build the CGI view, so they drop the field names that alias a CGI variable.
    pub superglobals: bool,
    pub write_timeout: Duration,
    pub keepalive_timeout: Duration,
    /// `[http].middleware` in config order, the first listed outermost.
    pub middleware: Vec<middleware::Layer>,
    /// Multipart limits of a dispatcher pool; None in the other modes, which feed php-src's own rfc1867 through read_post. `serve` replaces `dir` with the worker's own spool dir under it.
    pub uploads: Option<multipart::Limits>,
    /// sendFile() containment root.
    pub sendfile_root: PathBuf,
}

pub struct Server {
    config: Config,
    prepared: PreparedListener,
}

/// Runs in the master before the fork, with no runtime.
fn prepare(config: &Config) -> Result<PreparedListener> {
    // Before the fork: the PHP thread of a worker can read the identity at boot, before serve() runs.
    php::set_server_identity(config.server_name, config.server_port);
    php::set_sendfile_root(config.sendfile_root.clone());
    if let Some(uploads) = &config.uploads {
        multipart::sweep_spool_dirs(&uploads.dir);
    }
    let prepared = rapira_net::bind(&config.listen)?;
    tracing::info!(target: "http", "prepared listener on {}", prepared.addr());
    Ok(prepared)
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "http"
    }

    fn modes(&self) -> &'static [Mode] {
        &[Mode::Classic, Mode::Worker, Mode::Dispatcher]
    }

    fn dispatcher(&self) -> DispatcherClasses {
        php::DISPATCHER_CLASSES
    }

    fn serve(self: Box<Self>, worker: Worker) -> Result<()> {
        let Self { config, prepared } = *self;
        serve::serve(config, prepared, worker)
    }
}
