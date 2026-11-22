//! The http plugin. It serves HTTP/1.1 on the configured listener, and PHP answers every request.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use rapira_net::{ListenAddr, PreparedListener};
use rapira_sapi::plugin::{Mode, Plugin, ServeFuture, Worker};
use rapira_sapi::rapira_plugin;

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
    /// `[http].middleware` in config order. The first entry is outermost.
    pub middleware: Vec<middleware::Layer>,
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

/// Prepares the spool directory and the values that PHP can read at boot.
fn prepare(config: &mut Config) -> Result<Option<multipart::SpoolDir>> {
    let spool = config
        .uploads
        .as_mut()
        .map(multipart::SpoolDir::prepare)
        .transpose()?;
    php::set_server_identity(config.server_name, config.server_port);
    php::set_sendfile_root(config.sendfile_root.clone());
    Ok(spool)
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "http"
    }

    fn modes(&self) -> &'static [Mode] {
        &[Mode::Classic, Mode::Worker, Mode::Dispatcher]
    }

    fn dispatcher(&self) -> *const rapira_plugin {
        &raw const php::rapira_http_plugin
    }

    fn serve(self: Box<Self>, worker: Worker) -> ServeFuture {
        Box::pin(serve::serve(*self, worker))
    }
}
