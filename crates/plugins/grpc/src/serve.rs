use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use connectrpc::server::serve_connection;
use connectrpc::{
    Chain, CompressionRegistry, ConnectRpcService, ConnectionConfig, ConnectionInfo,
    DeadlinePolicy, GzipProvider, Router,
};
use connectrpc_health::StaticChecker;
use extension_api::{Addr, ListenAddr, Php, PreparedListener, Result};
use rapira_net::{Acceptor, Serve, StopHandle};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::Config;
use crate::dispatch::PhpDispatcher;

/// Everything the accept loop hands to a connection, and the drain that follows it.
struct Serving {
    service: ConnectRpcService<Chain<Router, PhpDispatcher>>,
    connection: ConnectionConfig,
    health: Arc<StaticChecker>,
    /// Each connection holds a receiver until it ends, so `closed()` resolves when the last connection is gone.
    shutdown: watch::Sender<bool>,
}

impl Serving {
    fn start(php: Php, config: &Config, router: Router, health: Arc<StaticChecker>) -> Self {
        match &config.listen {
            ListenAddr::Tcp(a) => tracing::info!(target: "grpc", "listening on {a}"),
            ListenAddr::Unix(p) => {
                tracing::info!(target: "grpc", "listening on unix:{}", p.display())
            }
        }
        let mut deadlines = DeadlinePolicy::new();
        if let Some(timeout) = config.default_timeout {
            deadlines = deadlines.with_default_timeout(timeout);
        }
        if let Some(max) = config.max_timeout {
            deadlines = deadlines.with_max(max);
        }
        let dispatcher = PhpDispatcher {
            schema: Arc::clone(&config.schema),
            php,
        };
        // The host routes come first, so a configured service cannot hide health or reflection.
        let service = ConnectRpcService::new(Chain(router, dispatcher))
            .with_deadline_policy(deadlines)
            // The default registry offers every codec that the build compiles, zstd included.
            .with_compression(CompressionRegistry::new().register(GzipProvider::default()));
        Self {
            service,
            connection: ConnectionConfig::new(),
            health,
            shutdown: watch::Sender::new(false),
        }
    }

    fn spawn<I>(&self, io: I, info: ConnectionInfo)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let open = self.shutdown.subscribe();
        let mut stop = open.clone();
        let connection = serve_connection(
            io,
            info,
            self.service.clone(),
            self.connection.clone(),
            async move {
                let _ = stop.wait_for(|stop| *stop).await;
            },
        );
        tokio::spawn(async move {
            // The shutdown future drops its receiver when shutdown starts, and the calls in flight continue after that. `open` keeps `closed()` pending until the connection ends.
            let closed = connection.await;
            drop(open);
            tracing::debug!(target: "grpc", "connection closed: {:?}", closed.reason());
        });
    }

    /// Waits out the connections in flight. The acceptor is already gone.
    async fn drain(self, fatal: Option<anyhow::Error>, grace: Duration) -> Result<()> {
        // `StaticChecker::shutdown` only sets NOT_SERVING. A `Watch` stream ends only when its service is removed, and an open stream holds its connection until the grace ends.
        self.health.shutdown();
        for name in self.health.services() {
            self.health.remove_service(&name);
        }
        self.shutdown.send_replace(true);
        let drained = tokio::time::timeout(grace, self.shutdown.closed())
            .await
            .is_ok();
        if let Some(e) = fatal {
            return Err(e);
        }
        if !drained {
            return Err(anyhow!(
                "grpc drain timed out after {grace:?}; open connections were cut"
            ));
        }
        tracing::info!(target: "grpc", "drained cleanly; accept loop stopped");
        Ok(())
    }
}

impl Serve for Serving {
    fn spawn_tcp(&self, stream: tokio::net::TcpStream, peer: std::net::SocketAddr) {
        let mut info = ConnectionInfo::new().with_peer_addr(peer);
        info.extensions_mut().insert(Addr::Inet(peer));
        self.spawn(stream, info);
    }

    fn spawn_unix(&self, stream: tokio::net::UnixStream, peer: Option<&std::path::Path>) {
        let mut info = ConnectionInfo::new();
        info.extensions_mut()
            .insert(Addr::Unix(peer.map(Into::into)));
        self.spawn(stream, info);
    }
}

/// Runs the accept loop on the calling thread, then drains the connections.
pub(crate) fn serve(
    php: Php,
    config: Config,
    prepared: PreparedListener,
    router: Router,
    health: Arc<StaticChecker>,
    stop: StopHandle,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let acceptor = Acceptor::adopt(prepared, stop, rt)?;
    let serving = Serving::start(php, &config, router, health);
    let fatal = acceptor.run(rt, &serving);
    rt.block_on(serving.drain(fatal, config.drain_grace))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use extension_api::Extension as _;
    use http::Method;

    use super::*;
    use crate::testing::{
        Answer, Conn, FakePhp, Fields, HI, HI_FRAME, Wire, config, envelope, fields, grpc_status,
        start, status_details, tcp, web_trailers,
    };

    #[derive(Clone, Copy)]
    enum On {
        Tcp,
        Unix,
    }

    /// Sources: PROTOCOL-HTTP2 (envelope, `grpc-status`, 12 for an unknown method or encoding), PROTOCOL-WEB (the 0x80 trailer frame), the Connect protocol (unary request, GET for no-side-effects methods, 404 for an unknown procedure), and proto3 JSON.
    #[tokio::test]
    async fn each_protocol_reaches_php_over_the_wire() {
        struct Case {
            name: &'static str,
            on: On,
            wire: Wire,
            method: Method,
            path: &'static str,
            headers: Fields,
            body: &'static [u8],
            status: u16,
            grpc_status: Option<&'static str>,
            content_type: Option<&'static str>,
            /// None: not checked.
            reply: Option<&'static [u8]>,
            /// The call reaches PHP.
            php: bool,
        }
        let grpc: Fields = &[("content-type", "application/grpc"), ("te", "trailers")];
        let cases = [
            Case {
                name: "grpc over h2c tcp",
                on: On::Tcp,
                wire: Wire::H2,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Echo",
                headers: grpc,
                body: HI_FRAME,
                status: 200,
                grpc_status: Some("0"),
                content_type: None,
                reply: Some(HI_FRAME),
                php: true,
            },
            Case {
                name: "grpc over h2c unix",
                on: On::Unix,
                wire: Wire::H2,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Echo",
                headers: grpc,
                body: HI_FRAME,
                status: 200,
                grpc_status: Some("0"),
                content_type: None,
                reply: Some(HI_FRAME),
                php: true,
            },
            Case {
                name: "grpc-web binary over http/1.1",
                on: On::Tcp,
                wire: Wire::Http1,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Echo",
                headers: &[("content-type", "application/grpc-web+proto")],
                body: HI_FRAME,
                status: 200,
                grpc_status: None,
                content_type: None,
                reply: Some(
                    b"\x00\x00\x00\x00\x04\x0a\x02hi\x80\x00\x00\x00\x10grpc-status: 0\r\n",
                ),
                php: true,
            },
            Case {
                name: "connect proto",
                on: On::Tcp,
                wire: Wire::Http1,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Echo",
                headers: &[("content-type", "application/proto")],
                body: HI,
                status: 200,
                grpc_status: None,
                content_type: Some("application/proto"),
                reply: Some(HI),
                php: true,
            },
            Case {
                name: "connect json",
                on: On::Tcp,
                wire: Wire::Http1,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Echo",
                headers: &[("content-type", "application/json")],
                body: br#"{"text":"hi"}"#,
                status: 200,
                grpc_status: None,
                content_type: Some("application/json"),
                reply: Some(br#"{"text":"hi"}"#),
                php: true,
            },
            Case {
                name: "connect get, idempotent",
                on: On::Tcp,
                wire: Wire::Http1,
                method: Method::GET,
                path: "/rapira.test.v1.EchoService/Get?encoding=json&message=%7B%22text%22%3A%22hi%22%7D",
                headers: &[],
                body: b"",
                status: 200,
                grpc_status: None,
                content_type: Some("application/json"),
                reply: Some(br#"{"text":"hi"}"#),
                php: true,
            },
            Case {
                name: "connect get, side effects",
                on: On::Tcp,
                wire: Wire::Http1,
                method: Method::GET,
                path: "/rapira.test.v1.EchoService/Echo?encoding=json&message=%7B%22text%22%3A%22hi%22%7D",
                headers: &[],
                body: b"",
                status: 405,
                grpc_status: None,
                content_type: None,
                reply: None,
                php: false,
            },
            Case {
                name: "streaming method",
                on: On::Tcp,
                wire: Wire::H2,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Watch",
                headers: grpc,
                body: HI_FRAME,
                status: 200,
                grpc_status: Some("12"),
                content_type: None,
                reply: Some(b""),
                php: false,
            },
            Case {
                name: "unknown method over grpc",
                on: On::Tcp,
                wire: Wire::H2,
                method: Method::POST,
                path: "/x.Y/Z",
                headers: grpc,
                body: HI_FRAME,
                status: 200,
                grpc_status: Some("12"),
                content_type: None,
                reply: Some(b""),
                php: false,
            },
            Case {
                name: "unknown method over connect",
                on: On::Tcp,
                wire: Wire::Http1,
                method: Method::POST,
                path: "/x.Y/Z",
                headers: &[("content-type", "application/proto")],
                body: HI,
                status: 404,
                grpc_status: None,
                content_type: None,
                reply: None,
                php: false,
            },
            Case {
                name: "zstd request",
                on: On::Tcp,
                wire: Wire::H2,
                method: Method::POST,
                path: "/rapira.test.v1.EchoService/Echo",
                headers: &[
                    ("content-type", "application/grpc"),
                    ("te", "trailers"),
                    ("grpc-encoding", "zstd"),
                ],
                body: b"\x01\x00\x00\x00\x04\x28\xb5\x2f\xfd",
                status: 200,
                grpc_status: Some("12"),
                content_type: None,
                reply: Some(b""),
                php: false,
            },
        ];

        let php = FakePhp::new(Answer::Echo);
        let dir = tempfile::tempdir().unwrap();
        let mut on_tcp = start(config(tcp()), php.clone()).await;
        let unix = ListenAddr::Unix(dir.path().join("grpc.sock"));
        let mut on_unix = start(config(unix), php.clone()).await;

        for case in cases {
            let listen = match case.on {
                On::Tcp => &on_tcp.listen,
                On::Unix => &on_unix.listen,
            };
            let mut conn = Conn::open(listen, case.wire).await;
            let before = php.seen();
            let got = conn
                .send(case.method, case.path, case.headers, case.body)
                .await
                .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

            assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
            assert_eq!(
                grpc_status(&got),
                case.grpc_status,
                "{}: {got:?}",
                case.name
            );
            if let Some(content_type) = case.content_type {
                assert_eq!(
                    got.headers.get("content-type").map(|v| v.as_bytes()),
                    Some(content_type.as_bytes()),
                    "{}",
                    case.name
                );
            }
            if let Some(reply) = case.reply {
                assert_eq!(&got.body[..], reply, "{}", case.name);
            }
            let calls = php.calls.lock().unwrap();
            assert_eq!(calls.len() - before, usize::from(case.php), "{}", case.name);
            if case.php {
                assert_eq!(calls[before].remote, conn.peer, "{}", case.name);
            }
        }

        on_tcp.server.shutdown().await.unwrap();
        on_unix.server.shutdown().await.unwrap();
    }

    /// Sources: `google/rpc/status.proto` (code 1, message 2, details 3), `google/protobuf/any.proto` (type_url 1, value 2), PROTOCOL-HTTP2 and PROTOCOL-WEB (status fields), the Connect protocol (error codes, error details with a bare type name, unpadded base64 values, `trailer-` fields of a unary response).
    #[tokio::test]
    async fn errors_and_metadata_encode_per_protocol() {
        struct Case {
            name: &'static str,
            wire: Wire,
            content_type: &'static str,
            body: &'static [u8],
            status: u16,
            headers: Fields,
            /// The status fields: HTTP trailers for gRPC, the 0x80 frame for gRPC-Web.
            trailers: Fields,
            details: bool,
            /// None: not checked.
            reply: Option<&'static [u8]>,
        }
        let status_fields: Fields = &[
            ("grpc-status", "5"),
            ("grpc-message", "no invoice"),
            ("x-t", "w"),
            ("x-b-bin", "AQI"),
        ];
        let cases = [
            Case {
                name: "grpc",
                wire: Wire::H2,
                content_type: "application/grpc",
                body: HI_FRAME,
                status: 200,
                headers: &[("x-h", "v")],
                trailers: status_fields,
                details: true,
                reply: Some(b""),
            },
            Case {
                name: "grpc-web",
                wire: Wire::Http1,
                content_type: "application/grpc-web+proto",
                body: HI_FRAME,
                status: 200,
                headers: &[("x-h", "v")],
                trailers: status_fields,
                details: true,
                reply: None,
            },
            Case {
                name: "connect",
                wire: Wire::Http1,
                content_type: "application/proto",
                body: HI,
                status: 404,
                headers: &[("x-h", "v"), ("trailer-x-t", "w"), ("trailer-x-b-bin", "AQI")],
                trailers: &[],
                details: false,
                reply: Some(
                    br#"{"code":"not_found","message":"no invoice","details":[{"type":"google.rpc.ErrorInfo","value":"CgF4"}]}"#,
                ),
            },
        ];
        let status_bytes = [
            &[0x08, 0x05, 0x12, 0x0a][..],
            b"no invoice",
            &[0x1a, 0x2f, 0x0a, 0x28],
            b"type.googleapis.com/google.rpc.ErrorInfo",
            &[0x12, 0x03, 0x0a, 0x01, 0x78],
        ]
        .concat();

        let php = FakePhp::new(Answer::Fail);
        let mut running = start(config(tcp()), php.clone()).await;
        for case in cases {
            let mut conn = Conn::open(&running.listen, case.wire).await;
            let got = conn
                .send(
                    Method::POST,
                    "/rapira.test.v1.EchoService/Echo",
                    &[("content-type", case.content_type), ("te", "trailers")],
                    case.body,
                )
                .await
                .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));

            assert_eq!(got.status, case.status, "{}: {got:?}", case.name);
            let trailers = if case.content_type.starts_with("application/grpc-web") {
                web_trailers(&got.body)
            } else {
                got.trailers.clone()
            };
            for (name, want) in fields(case.headers).iter() {
                assert_eq!(
                    got.headers.get(name),
                    Some(want),
                    "{}: {name}: {got:?}",
                    case.name
                );
            }
            for (name, want) in fields(case.trailers).iter() {
                assert_eq!(
                    trailers.get(name),
                    Some(want),
                    "{}: {name}: {trailers:?}",
                    case.name
                );
            }
            let details = status_details(&trailers);
            assert_eq!(
                details.as_deref(),
                case.details.then_some(&status_bytes[..]),
                "{}",
                case.name
            );
            if let Some(reply) = case.reply {
                assert_eq!(
                    &got.body[..],
                    reply,
                    "{}: {}",
                    case.name,
                    String::from_utf8_lossy(&got.body)
                );
            }
        }
        running.server.shutdown().await.unwrap();
    }

    /// Sources: the contract gRPC README (the host enforces `Call\Context::$deadline`) and PROTOCOL-HTTP2 `grpc-timeout` (`200m` is 200 ms, status 4 is DEADLINE_EXCEEDED).
    #[tokio::test]
    async fn a_passed_deadline_drops_the_call() {
        let php = FakePhp::new(Answer::Never);
        let mut running = start(config(tcp()), php.clone()).await;
        let mut conn = Conn::open(&running.listen, Wire::H2).await;
        let got = conn
            .grpc(
                "/rapira.test.v1.EchoService/Echo",
                &[("grpc-timeout", "200m")],
                HI_FRAME,
            )
            .await
            .unwrap();

        assert_eq!(grpc_status(&got), Some("4"), "{got:?}");
        assert!(
            php.dropped.load(Ordering::Acquire),
            "the call must be dropped"
        );
        running.server.shutdown().await.unwrap();
    }

    /// Sources: `grpc/health/v1/health.proto` (`HealthCheckRequest.service` and `HealthCheckResponse.status` are field 1, SERVING is 1), `grpc/reflection/v1/reflection.proto` (`list_services` is field 7), and PROTOCOL-HTTP2 (12 for an unknown method).
    #[tokio::test]
    async fn health_and_reflection_answer_from_the_host() {
        struct Case {
            name: &'static str,
            reflection: bool,
            path: &'static str,
            message: &'static [u8],
            grpc_status: &'static str,
            /// Bytes that the response body contains.
            reply: &'static [u8],
        }
        const REFLECTION: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
        let cases = [
            Case {
                name: "health check, whole server",
                reflection: false,
                path: "/grpc.health.v1.Health/Check",
                message: b"",
                grpc_status: "0",
                reply: b"\x00\x00\x00\x00\x02\x08\x01",
            },
            Case {
                name: "health check, configured service",
                reflection: false,
                path: "/grpc.health.v1.Health/Check",
                message: b"\x0a\x1arapira.test.v1.EchoService",
                grpc_status: "0",
                reply: b"\x00\x00\x00\x00\x02\x08\x01",
            },
            Case {
                name: "reflection off",
                reflection: false,
                path: REFLECTION,
                message: b"\x3a\x00",
                grpc_status: "12",
                reply: b"",
            },
            Case {
                name: "reflection on",
                reflection: true,
                path: REFLECTION,
                message: b"\x3a\x00",
                grpc_status: "0",
                reply: b"rapira.test.v1.EchoService",
            },
        ];

        let php = FakePhp::new(Answer::Echo);
        let mut plain = start(config(tcp()), php.clone()).await;
        let reflecting = Config {
            reflection: true,
            ..config(tcp())
        };
        let mut reflecting = start(reflecting, php.clone()).await;
        for case in cases {
            let listen = if case.reflection {
                &reflecting.listen
            } else {
                &plain.listen
            };
            let mut conn = Conn::open(listen, Wire::H2).await;
            let got = conn
                .grpc(case.path, &[], &envelope(case.message))
                .await
                .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));
            assert_eq!(
                grpc_status(&got),
                Some(case.grpc_status),
                "{}: {got:?}",
                case.name
            );
            assert!(
                case.reply.is_empty()
                    || got.body.windows(case.reply.len()).any(|w| w == case.reply),
                "{}: {got:?}",
                case.name
            );
        }
        assert_eq!(php.seen(), 0, "the host answers without PHP");
        plain.server.shutdown().await.unwrap();
        reflecting.server.shutdown().await.unwrap();
    }
}
