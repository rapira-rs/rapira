use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Extension, ListenAddr, Php, PrepareCtx, PreparedListener, Result};
use rapira_net::{Stop, join_thread};
use tokio::runtime::Builder;

mod dispatch;
mod schema;
mod serve;
#[cfg(test)]
mod testing;

pub use schema::{MethodInfo, Schema, ServiceInfo};

#[derive(Clone)]
pub struct Config {
    pub listen: ListenAddr,
    pub schema: Arc<Schema>,
    /// The timeout of a call whose client sets no deadline.
    pub default_timeout: Option<Duration>,
    /// The longest timeout that a client can set.
    pub max_timeout: Option<Duration>,
    pub drain_grace: Duration,
}

pub struct Server {
    config: Config,
    prepared: Option<PreparedListener>,
    stop: Option<Stop>,
    join: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Extension for Server {
    type Config = Config;

    fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            stop: None,
            join: None,
        }
    }

    fn name(&self) -> &str {
        "rapira-grpc"
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        let prepared = match &self.config.listen {
            ListenAddr::Tcp(addr) => ctx.bind_tcp(*addr)?,
            ListenAddr::Unix(path) => ctx.bind_unix(path)?,
        };
        match prepared.addr() {
            ListenAddr::Tcp(a) => tracing::info!(target: "grpc", "prepared listener on {a}"),
            ListenAddr::Unix(p) => {
                tracing::info!(target: "grpc", "prepared listener on {}", p.display());
            }
        }
        self.prepared = Some(prepared);
        Ok(())
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let config = self.config.clone();
        let Some(prepared) = self.prepared.take() else {
            return Err(anyhow!("grpc listener was not prepared"));
        };
        let stop = Stop::new().map_err(|e| anyhow!("creating the grpc stop handle: {e}"))?;
        let handle = stop.handle();

        let thread = std::thread::Builder::new()
            .name("rapira-grpc".into())
            .spawn(move || {
                let rt = Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("rapira-grpc-io")
                    .build()
                    .map_err(|e| anyhow!("building the grpc runtime: {e}"))?;
                serve::serve(php, config, prepared, handle, &rt)
            })?;

        self.stop = Some(stop);
        let join = self.join.insert(tokio::task::spawn_blocking(move || {
            join_thread(thread, "grpc")
        }));
        let result = join.await;
        self.join = None;
        result.map_err(|e| anyhow!("grpc join task failed: {e}"))?
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            stop.stop();
        }
        if let Some(join) = self.join.take() {
            join.await
                .map_err(|e| anyhow!("grpc join task failed: {e}"))??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;
    use std::task::Poll;

    use super::*;
    use crate::testing::{
        Answer, Conn, FakePhp, HI_FRAME, Wire, config, grpc_status, start, tcp, wait_until,
    };

    /// Mirrors the HTTP plugin: the host drops `run`, then `shutdown` joins the thread and frees every `Php` clone.
    #[tokio::test]
    async fn shutdown_joins_the_server_after_run_is_cancelled() {
        let mut server = Server::init(config(tcp()));
        let mut ctx = PrepareCtx::new();
        server.prepare(&mut ctx).unwrap();
        let backend = FakePhp::new(Answer::Echo);
        let php = Php::new(backend.clone());

        let mut run = Box::pin(server.run(php));
        assert!(std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx).is_pending())).await);
        drop(run);

        assert!(server.join.is_some());
        server.shutdown().await.unwrap();
        assert!(server.join.is_none());
        assert_eq!(Arc::strong_count(&backend), 1);
    }

    #[tokio::test]
    async fn drain_waits_for_calls_within_the_grace() {
        struct Case {
            name: &'static str,
            answer: Answer,
            grace: Duration,
            grpc_status: Option<&'static str>,
            /// A text of the shutdown error. None: shutdown succeeds.
            shutdown: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "a call that ends inside the grace completes",
                answer: Answer::Late(Duration::from_millis(100)),
                grace: Duration::from_secs(2),
                grpc_status: Some("0"),
                shutdown: None,
            },
            Case {
                name: "a call past the grace is cut",
                answer: Answer::Never,
                grace: Duration::from_millis(200),
                grpc_status: None,
                shutdown: Some("grpc drain timed out"),
            },
        ];
        for case in cases {
            let php = FakePhp::new(case.answer);
            let config = Config {
                drain_grace: case.grace,
                ..config(tcp())
            };
            let mut running = start(config, php.clone()).await;
            let mut conn = Conn::open(&running.listen, Wire::H2).await;
            let call = tokio::spawn(async move {
                conn.grpc("/rapira.test.v1.EchoService/Echo", &[], HI_FRAME)
                    .await
            });
            wait_until(|| php.seen() == 1).await;

            let shutdown = running.server.shutdown().await;
            let got = call.await.unwrap();
            assert_eq!(
                got.as_ref().ok().and_then(grpc_status),
                case.grpc_status,
                "{}: {got:?}",
                case.name
            );
            match case.shutdown {
                None => assert!(shutdown.is_ok(), "{}: {shutdown:?}", case.name),
                Some(text) => assert!(
                    shutdown
                        .as_ref()
                        .is_err_and(|e| e.to_string().contains(text)),
                    "{}: {shutdown:?}",
                    case.name
                ),
            }
        }
    }
}
