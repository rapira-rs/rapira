use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use rapira_net::{ListenAddr, PrepareCtx, PreparedListener};
use rapira_sapi::plugin::{Mode, PhpPart, Plugin, Worker};
use rapira_sapi::work::Intake;

use exchange::Exchange;

mod bridge;
mod check;
pub mod config;
mod exchange;
mod handler;
pub mod middleware;
pub mod multipart;
mod php;
mod request;
mod response;
mod serve;

pub use php::PHP_PART;

#[derive(Clone)]
pub(crate) struct Config {
    pub listen: ListenAddr,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub unsafe_field_names: UnsafeFieldNames,
    pub superglobals: bool,
    pub write_timeout: Duration,
    pub keepalive_timeout: Duration,
    /// `[http].middleware` in config order, the first listed outermost.
    pub middleware: Vec<middleware::Layer>,
    /// Multipart limits of a dispatcher pool. Each worker spools in its own dir under `dir`, which `serve` creates. None: the default limits.
    pub uploads: Option<multipart::Limits>,
    /// sendFile() containment root.
    pub sendfile_root: PathBuf,
}

/// The `HTTP_*` mapping rewrites `-` to `_` and PHP rewrites `.` to `_`, so `X_Forwarded_For` and `X.Forwarded.For` both land on `HTTP_X_FORWARDED_FOR`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnsafeFieldNames {
    #[default]
    Drop,
    Reject,
}

pub struct Server {
    config: Config,
    prepared: Option<PreparedListener>,
}

impl Server {
    pub(crate) fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
        }
    }
}

impl Plugin for Server {
    fn name(&self) -> &'static str {
        "http"
    }

    fn modes(&self) -> &'static [Mode] {
        &[Mode::Classic, Mode::Worker, Mode::Dispatcher]
    }

    fn php(&self) -> Option<PhpPart> {
        Some(PHP_PART)
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        if let Some(uploads) = &self.config.uploads {
            multipart::sweep_spool_dirs(&uploads.dir);
        }
        let prepared = ctx.bind(&self.config.listen)?;
        tracing::info!(target: "http", "prepared listener on {}", prepared.addr());
        self.prepared = Some(prepared);
        Ok(())
    }

    fn serve(self: Box<Self>, worker: Worker) -> Result<()> {
        let Self { config, prepared } = *self;
        let Some(prepared) = prepared else {
            return Err(anyhow!("http listener was not prepared"));
        };
        php::set_sendfile_root(config.sendfile_root.clone());
        let intake = Intake::new(worker.sink.clone());
        serve::serve(intake, config, prepared, worker)
    }
}
