use std::net::{SocketAddr, TcpListener};
use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use extension_api::{ListenAddr, PrepareCtx};
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use php_sys::{Mode, Rapira};
use rapira_runtime::ExtensionRuntime;
use serde_json::Value;

use crate::harness::{fixture_path, scratch_dir};

type Fields = &'static [(&'static str, &'static str)];

const ECHO: &str = "/rapira.test.v1.EchoService/Echo";

/// `EchoRequest { text }`: field 1, length-delimited, for a text shorter than 128 bytes.
fn echo_request(text: &str) -> Vec<u8> {
    let mut message = vec![0x0a, text.len() as u8];
    message.extend_from_slice(text.as_bytes());
    message
}

/// `message` in an uncompressed gRPC envelope.
fn envelope(message: &[u8]) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

fn fields(lines: Fields) -> HeaderMap {
    lines
        .iter()
        .map(|&(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
        .collect()
}

/// A rapira_grpc server on TCP and on a unix socket in this process. `echo-worker.php` answers.
struct Host {
    rapira: Rapira,
    running: rapira_runtime::Running,
    tcp: SocketAddr,
    unix: PathBuf,
    dir: PathBuf,
    _prepared: PrepareCtx,
}

impl Host {
    fn start() -> anyhow::Result<Host> {
        let dir = scratch_dir();
        let unix = dir.join("grpc.sock");
        let schema = Arc::new(rapira_grpc::Schema::load(
            &tests::echo_descriptor_set(),
            &["rapira.test.v1.EchoService".to_owned()],
        )?);
        let config = |listen| rapira_grpc::Config {
            listen,
            schema: Arc::clone(&schema),
            default_timeout: None,
            max_timeout: None,
            drain_grace: Duration::from_secs(5),
        };
        let mut host = ExtensionRuntime::new();
        host.register::<rapira_grpc::Server>(config(ListenAddr::Tcp(([127, 0, 0, 1], 0).into())));
        host.register::<rapira_grpc::Server>(config(ListenAddr::Unix(unix.clone())));
        let mut prepared = PrepareCtx::new();
        host.prepare_all(&mut prepared)?;
        // SAFETY: prepared owns the descriptor for the lifetime of this borrow.
        let listener = unsafe { BorrowedFd::borrow_raw(prepared.listener_fds()[0]) };
        let tcp = TcpListener::from(listener.try_clone_to_owned()?).local_addr()?;

        let script = fixture_path("grpc/echo-worker.php");
        let rapira = Rapira::start(Mode::GrpcDispatcher {
            script: script.clone(),
            services: tests::echo_services(),
        })?;
        let running = host.run(rapira.handle(), script);
        Ok(Host {
            rapira,
            running,
            tcp,
            unix,
            dir,
            _prepared: prepared,
        })
    }

    fn stop(self) -> anyhow::Result<()> {
        let outcomes = self.running.stop();
        drop(self.rapira);
        anyhow::ensure!(
            outcomes.iter().all(Result::is_ok),
            "gRPC shutdown: {outcomes:?}"
        );
        std::fs::remove_dir_all(self.dir)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Transport {
    H2Tcp,
    H2Unix,
    Http1Tcp,
}

#[derive(Debug)]
struct Response {
    status: u16,
    headers: HeaderMap,
    body: Bytes,
    trailers: HeaderMap,
}

impl Response {
    /// The `grpc-status` trailer, or the header of a trailers-only response.
    fn grpc_status(&self) -> Option<&str> {
        self.trailers
            .get("grpc-status")
            .or_else(|| self.headers.get("grpc-status"))
            .and_then(|v| v.to_str().ok())
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("client runtime")
}

/// One POST to `path` on a new connection.
async fn post(
    host: &Host,
    transport: Transport,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> anyhow::Result<Response> {
    let mut req = http::Request::post(match transport {
        Transport::Http1Tcp => path.to_owned(),
        Transport::H2Tcp | Transport::H2Unix => format!("http://localhost{path}"),
    })
    .header("host", "localhost");
    for &(k, v) in headers {
        req = req.header(k, v);
    }
    let req = req.body(Full::new(Bytes::from(body)))?;
    let resp = match transport {
        Transport::H2Tcp => {
            let io = TokioIo::new(tokio::net::TcpStream::connect(host.tcp).await?);
            let (mut send, conn) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await?;
            tokio::spawn(conn);
            send.send_request(req).await?
        }
        Transport::H2Unix => {
            let io = TokioIo::new(tokio::net::UnixStream::connect(&host.unix).await?);
            let (mut send, conn) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await?;
            tokio::spawn(conn);
            send.send_request(req).await?
        }
        Transport::Http1Tcp => {
            let io = TokioIo::new(tokio::net::TcpStream::connect(host.tcp).await?);
            let (mut send, conn) = hyper::client::conn::http1::handshake(io).await?;
            tokio::spawn(conn);
            send.send_request(req).await?
        }
    };
    let (parts, body) = resp.into_parts();
    let collected = body.collect().await?;
    let trailers = collected.trailers().cloned().unwrap_or_default();
    Ok(Response {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: collected.to_bytes(),
        trailers,
    })
}

/// Sources: PROTOCOL-HTTP2 (envelope and `grpc-status`), the Connect protocol and proto3 JSON.
#[test]
fn php_answers_unary_calls_over_each_transport() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        transport: Transport,
        content_type: &'static str,
        body: &'static [u8],
        status: u16,
        grpc_status: Option<&'static str>,
        reply: &'static [u8],
    }
    let hi_frame: &[u8] = b"\x00\x00\x00\x00\x04\x0a\x02hi";
    let cases = [
        Case {
            name: "grpc h2c tcp",
            transport: Transport::H2Tcp,
            content_type: "application/grpc",
            body: hi_frame,
            status: 200,
            grpc_status: Some("0"),
            reply: hi_frame,
        },
        Case {
            name: "grpc h2c unix",
            transport: Transport::H2Unix,
            content_type: "application/grpc",
            body: hi_frame,
            status: 200,
            grpc_status: Some("0"),
            reply: hi_frame,
        },
        Case {
            name: "connect json",
            transport: Transport::Http1Tcp,
            content_type: "application/json",
            body: br#"{"text":"hi"}"#,
            status: 200,
            grpc_status: None,
            reply: br#"{"text":"hi"}"#,
        },
    ];

    let _php = tests::php_lock();
    let host = Host::start()?;
    let rt = runtime();
    let result = (|| -> anyhow::Result<()> {
        for case in cases {
            let got = rt.block_on(post(
                &host,
                case.transport,
                ECHO,
                &[("content-type", case.content_type), ("te", "trailers")],
                case.body.to_vec(),
            ))?;
            anyhow::ensure!(
                got.status == case.status
                    && got.grpc_status() == case.grpc_status
                    && got.body == case.reply,
                "{}: {got:?}",
                case.name
            );
        }
        Ok(())
    })();
    // The client runtime holds the client connections. Dropping it closes them, so the drain does not wait on them.
    drop(rt);
    let stopped = host.stop();
    result.and(stopped)
}

/// Sources: the contract gRPC README (a status goes through `fail()` only, an uncaught throwable is a sanitized INTERNAL, the host enforces `Call\Context::$deadline`), decision D3 (a lost call is INTERNAL), `google/rpc/status.proto` with `google/protobuf/any.proto`, and RFC 4648: 00 ff is `AP8`.
#[test]
fn php_outcomes_reach_the_client() -> anyhow::Result<()> {
    struct Step {
        text: &'static str,
        headers: Fields,
        grpc_status: &'static str,
    }
    struct Case {
        name: &'static str,
        steps: &'static [Step],
        /// Fields of the last response: headers and trailers.
        headers: Fields,
        trailers: Fields,
        /// Check that the last response carries the `google.rpc.Status` of `fail`.
        details: bool,
        /// An app record that PHP leaves, and its context.
        log: Option<(&'static str, &'static str)>,
    }
    let cases = [
        Case {
            name: "fail with a detail",
            steps: &[Step {
                text: "fail",
                headers: &[],
                grpc_status: "5",
            }],
            headers: &[],
            trailers: &[],
            details: true,
            log: None,
        },
        Case {
            name: "a lost call is internal and the worker recovers",
            steps: &[
                Step {
                    text: "drop",
                    headers: &[],
                    grpc_status: "13",
                },
                Step {
                    text: "hi",
                    headers: &[],
                    grpc_status: "0",
                },
            ],
            headers: &[],
            trailers: &[],
            details: false,
            log: None,
        },
        Case {
            name: "an uncaught throwable is internal and the worker recovers",
            steps: &[
                Step {
                    text: "throw",
                    headers: &[],
                    grpc_status: "13",
                },
                Step {
                    text: "hi",
                    headers: &[],
                    grpc_status: "0",
                },
            ],
            headers: &[],
            trailers: &[],
            details: false,
            log: None,
        },
        Case {
            name: "the deadline cancels the php call",
            steps: &[Step {
                text: "slow",
                headers: &[("grpc-timeout", "300m")],
                grpc_status: "4",
            }],
            headers: &[],
            trailers: &[],
            details: false,
            log: Some((
                "slow",
                r#"{"cancelled":"yes","respond":"Rapira\\Exception\\WorkDiscardedException"}"#,
            )),
        },
        Case {
            name: "metadata crosses the edge",
            steps: &[Step {
                text: "meta",
                headers: &[("x-echo", "a"), ("x-echo-bin", "AP8")],
                grpc_status: "0",
            }],
            headers: &[("x-echo", "a")],
            trailers: &[("x-echo-bin", "AP8")],
            details: false,
            log: Some(("meta", r#"{"keys":["x-echo","x-echo-bin"]}"#)),
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

    let _php = tests::php_lock();
    tests::init_log_capture();
    tests::captured().clear();
    let host = Host::start()?;
    let rt = runtime();
    let result = (|| -> anyhow::Result<()> {
        for case in cases {
            let mut last = None;
            for step in case.steps {
                let mut headers = vec![("content-type", "application/grpc"), ("te", "trailers")];
                headers.extend_from_slice(step.headers);
                let got = rt.block_on(post(
                    &host,
                    Transport::H2Tcp,
                    ECHO,
                    &headers,
                    envelope(&echo_request(step.text)),
                ))?;
                anyhow::ensure!(
                    got.grpc_status() == Some(step.grpc_status),
                    "{}: {}: {got:?}",
                    case.name,
                    step.text
                );
                last = Some(got);
            }
            let last = last.expect("every case has a step");
            for (name, want) in fields(case.headers).iter() {
                anyhow::ensure!(
                    last.headers.get(name) == Some(want),
                    "{}: header {name}: {last:?}",
                    case.name
                );
            }
            for (name, want) in fields(case.trailers).iter() {
                anyhow::ensure!(
                    last.trailers.get(name) == Some(want),
                    "{}: trailer {name}: {last:?}",
                    case.name
                );
            }
            if case.details {
                let details = last
                    .trailers
                    .get("grpc-status-details-bin")
                    .map(|v| STANDARD_NO_PAD.decode(v.as_bytes()))
                    .transpose()?;
                anyhow::ensure!(
                    details.as_deref() == Some(&status_bytes[..]),
                    "{}: grpc-status-details-bin {details:?}",
                    case.name
                );
            }
            if let Some((message, context)) = case.log {
                let got: Value = serde_json::from_str(&tests::wait_app_record(message))?;
                let want: Value = serde_json::from_str(context)?;
                anyhow::ensure!(got == want, "{}: {message} record {got}", case.name);
            }
        }
        Ok(())
    })();
    // The client runtime holds the client connections. Dropping it closes them, so the drain does not wait on them.
    drop(rt);
    let stopped = host.stop();
    result.and(stopped)
}
