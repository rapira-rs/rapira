use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use rapira_net::{ListenAddr, PrepareCtx, PreparedListener};
use rapira_sapi::http::Exchange;
use rapira_sapi::middleware::Middleware;
use rapira_sapi::multipart;
use rapira_sapi::plugin::{Mode, Plugin, Worker};
use rapira_sapi::work::{DispatcherClasses, Intake};

mod bridge;
mod check;
mod handler;
mod request;
mod response;
mod serve;

#[derive(Clone)]
pub struct Config {
    pub listen: ListenAddr,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub unsafe_field_names: UnsafeFieldNames,
    pub superglobals: bool,
    pub write_timeout: Duration,
    pub drain_grace: Duration,
    pub keepalive_timeout: Duration,
    pub middleware: Vec<Arc<dyn Middleware>>,
    /// Multipart limits of a dispatcher pool. Each worker spools in `multipart::worker_spool_dir(dir)`, which the worker creates. None: the default limits.
    pub uploads: Option<multipart::Limits>,
    /// sendFile() containment root.
    pub sendfile_root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeFieldNames {
    Drop,
    Reject,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ListenAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000))),
            server_name: "localhost".to_owned(),
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
            unsafe_field_names: UnsafeFieldNames::Drop,
            superglobals: true,
            write_timeout: Duration::from_secs(30),
            drain_grace: Duration::from_secs(25),
            keepalive_timeout: Duration::from_secs(60),
            middleware: Vec::new(),
            uploads: None,
            sendfile_root: PathBuf::from("."),
        }
    }
}

pub struct Server {
    config: Config,
    prepared: Option<PreparedListener>,
    /// The intake a test injects. None: the worker's sink.
    intake: Option<Intake<Exchange>>,
}

impl Server {
    pub fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            intake: None,
        }
    }

    /// A server that submits to `intake` in place of the worker's sink.
    pub fn with_intake(config: Config, intake: Intake<Exchange>) -> Self {
        Self {
            intake: Some(intake),
            ..Self::init(config)
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

    fn dispatcher_classes(&self) -> Option<DispatcherClasses> {
        Some(rapira_sapi::http::DISPATCHER_CLASSES)
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        let prepared = ctx.bind(&self.config.listen)?;
        tracing::info!(target: "http", "prepared listener on {}", prepared.addr());
        self.prepared = Some(prepared);
        Ok(())
    }

    fn serve(self: Box<Self>, worker: Worker) -> Result<()> {
        let Self {
            config,
            prepared,
            intake,
        } = *self;
        let Some(prepared) = prepared else {
            return Err(anyhow!("http listener was not prepared"));
        };
        rapira_sapi::set_sendfile_root(config.sendfile_root.clone());
        let intake = intake.unwrap_or_else(|| Intake::new(worker.sink.clone()));
        serve::serve(intake, config, prepared, worker)
    }
}

#[cfg(test)]
mod tests {
    use rapira_sapi::plugin::run_plugin;
    use rapira_sapi::work::Sink;

    use super::*;

    /// Stop ends the accept loop and the drain, and the plugin thread drops every intake clone and the listener.
    #[test]
    fn stop_joins_the_server_and_drops_every_intake_clone() {
        let (intake, mut units) = Intake::<Exchange>::channel(1);
        let mut server = Server::with_intake(
            Config {
                listen: ListenAddr::Tcp(([127, 0, 0, 1], 0).into()),
                ..Config::default()
            },
            intake,
        );
        let mut ctx = PrepareCtx::new();
        server.prepare(&mut ctx).unwrap();
        let fd = ctx.listener_fds()[0];
        // SAFETY: `ctx` owns the descriptor while it is borrowed here.
        let listener = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let addr = std::net::TcpListener::from(listener.try_clone_to_owned().unwrap())
            .local_addr()
            .unwrap();
        drop(ctx);
        let (sink, _php) = Sink::channel(1);
        let running = run_plugin(
            Box::new(server),
            sink,
            Duration::from_secs(5),
            PathBuf::from("index.php"),
            Mode::Dispatcher,
        )
        .unwrap();
        std::net::TcpStream::connect(addr).expect("the server accepts before stop");

        running.stop();
        running.join().unwrap();

        assert!(
            matches!(
                units.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ),
            "every intake clone is gone"
        );
        let refused = std::net::TcpStream::connect(addr);
        assert!(refused.is_err(), "the listener is closed: {refused:?}");
    }
}
