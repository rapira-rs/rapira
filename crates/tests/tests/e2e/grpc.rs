use std::net::SocketAddr;

use http::Method;
use rapira_net::{ListenAddr, PrepareCtx};
use rapira_sapi::Rapira;
use rapira_sapi::plugin::{Mode, Plugin as _, run_plugin};
use serde_json::Value;
use tests::grpc::{Conn, ECHO_PATH as ECHO, Fields, Wire, config, envelope, fields, tcp_addr};

use crate::harness::{
    BOOT, ECHO_SERVICE, MASTER_EXIT_OK, STOP_BUDGET, assert_exit_code, diagnostics, fixture_path,
    http_get, signal, spawn_grpc, spawn_grpc_boot_failure, spawn_grpc_with_http, wait_log_contains,
    wait_workers,
};

/// `EchoRequest { text }`: field 1, length-delimited, for a text shorter than 128 bytes.
fn echo_request(text: &str) -> Vec<u8> {
    let mut message = vec![0x0a, text.len() as u8];
    message.extend_from_slice(text.as_bytes());
    message
}

/// A rapira_grpc server in this process. `echo-worker.php` answers.
struct Server {
    rapira: Rapira,
    running: rapira_sapi::plugin::Running,
    tcp: ListenAddr,
    _prepared: PrepareCtx,
}

impl Server {
    fn start() -> anyhow::Result<Server> {
        let mut server =
            rapira_grpc::Server::init(config(ListenAddr::Tcp(([127, 0, 0, 1], 0).into())));
        let mut prepared = PrepareCtx::new();
        server.prepare(&mut prepared)?;
        let tcp = ListenAddr::Tcp(tcp_addr(&prepared, 0));

        let script = fixture_path("grpc/echo-worker.php");
        let rapira = tests::start_grpc(script.clone())?;
        let running = run_plugin(
            Box::new(server),
            rapira.sink(),
            std::time::Duration::from_secs(30),
            script,
            Mode::Dispatcher,
        )?;
        Ok(Server {
            rapira,
            running,
            tcp,
            _prepared: prepared,
        })
    }

    fn stop(self) -> anyhow::Result<()> {
        self.running.stop();
        let outcome = self.running.join();
        drop(self.rapira);
        outcome.map_err(|e| e.context("gRPC shutdown"))
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("client runtime")
}

/// The two paths that only real PHP behind the real listener proves: the deadline reaches PHP as a cancel, and metadata crosses the edge both ways. Sources: the contract gRPC README (the host enforces `Call\Context::$deadline`) and RFC 4648: 00 ff is `AP8`.
#[test]
fn php_outcomes_reach_the_client() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        text: &'static str,
        metadata: Fields,
        grpc_status: &'static str,
        headers: Fields,
        trailers: Fields,
        /// An app record that PHP leaves, and its context.
        log: (&'static str, &'static str),
    }
    let cases = [
        Case {
            name: "the deadline cancels the php call",
            text: "slow",
            metadata: &[("grpc-timeout", "300m")],
            grpc_status: "4",
            headers: &[],
            trailers: &[],
            log: (
                "slow",
                r#"{"cancelled":"yes","respond":"Rapira\\Exception\\WorkDiscardedException"}"#,
            ),
        },
        Case {
            name: "metadata crosses the edge",
            text: "meta",
            metadata: &[("x-echo", "a"), ("x-echo-bin", "AP8")],
            grpc_status: "0",
            headers: &[("x-echo", "a")],
            trailers: &[("x-echo-bin", "AP8")],
            log: ("meta", r#"{"keys":["x-echo","x-echo-bin"]}"#),
        },
    ];

    let _php = tests::php_lock();
    tests::init_log_capture();
    tests::captured().clear();
    let server = Server::start()?;
    let rt = runtime();
    let result = (|| -> anyhow::Result<()> {
        for case in cases {
            let got = rt.block_on(async {
                let mut conn = Conn::open(&server.tcp, Wire::H2).await?;
                conn.grpc(ECHO, case.metadata, &envelope(&echo_request(case.text)))
                    .await
            })?;
            anyhow::ensure!(
                got.grpc_status() == Some(case.grpc_status),
                "{}: {got:?}",
                case.name
            );
            for (name, want) in fields(case.headers).iter() {
                anyhow::ensure!(
                    got.headers.get(name) == Some(want),
                    "{}: header {name}: {got:?}",
                    case.name
                );
            }
            for (name, want) in fields(case.trailers).iter() {
                anyhow::ensure!(
                    got.trailers.get(name) == Some(want),
                    "{}: trailer {name}: {got:?}",
                    case.name
                );
            }
            let (message, context) = case.log;
            let got: Value = serde_json::from_str(&tests::wait_app_record(message))?;
            let want: Value = serde_json::from_str(context)?;
            anyhow::ensure!(got == want, "{}: {message} record {got}", case.name);
        }
        Ok(())
    })();
    // The client runtime holds the client connections. Dropping it closes them, so the drain does not wait on them.
    drop(rt);
    let stopped = server.stop();
    result.and(stopped)
}

/// A Connect unary call with a JSON body over HTTP/1.1. The server may send the reply chunked, so hyper reads it.
fn connect_json(addr: SocketAddr, path: &str, body: &str) -> anyhow::Result<(u16, String)> {
    let call = async {
        let mut conn = Conn::open(&ListenAddr::Tcp(addr), Wire::Http1).await?;
        let got = conn
            .send(
                Method::POST,
                path,
                &[("content-type", "application/json")],
                body.as_bytes(),
            )
            .await?;
        anyhow::Ok((got.status, String::from_utf8_lossy(&got.body).into_owned()))
    };
    runtime().block_on(async { tokio::time::timeout(BOOT, call).await })?
}

/// Sources: the Connect protocol (a unary call posts the message as proto3 JSON) and `grpc/health/v1/health.proto`.
#[test]
fn grpc_pool_serves_from_rapira_toml() {
    struct Case {
        name: &'static str,
        path: &'static str,
        body: &'static str,
        reply: &'static str,
    }
    let cases = [
        Case {
            name: "php echoes",
            path: ECHO,
            body: r#"{"text":"hi"}"#,
            reply: r#"{"text":"hi"}"#,
        },
        Case {
            name: "the host answers health",
            path: "/grpc.health.v1.Health/Check",
            body: "{}",
            reply: r#"{"status":"SERVING"}"#,
        },
    ];
    let srv = spawn_grpc(2);
    wait_workers(&srv, BOOT, "2 grpc workers", |p| p.len() == 2);
    for case in cases {
        let got = connect_json(srv.addr, case.path, case.body);
        assert!(
            matches!(&got, Ok((200, reply)) if reply == case.reply),
            "{}: {got:?}\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// Each worker gets the dispatcher of its own pool: `echo-worker.php` answers HTTP, `grpc-worker.php` answers gRPC.
#[test]
fn http_and_grpc_pools_run_side_by_side() {
    let (srv, http) = spawn_grpc_with_http("shared/echo-worker.php");
    wait_workers(&srv, BOOT, "1 http and 1 grpc worker", |p| p.len() == 2);

    let (code, body) =
        http_get(http, "/", BOOT).unwrap_or_else(|e| panic!("GET /: {e}\n{}", diagnostics(&srv)));
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(
        body.starts_with(b"ok:"),
        "{:?}\n{}",
        String::from_utf8_lossy(&body),
        diagnostics(&srv)
    );

    let got = connect_json(srv.addr, ECHO, r#"{"text":"hi"}"#);
    assert!(
        matches!(&got, Ok((200, reply)) if reply == r#"{"text":"hi"}"#),
        "{got:?}\n{}",
        diagnostics(&srv)
    );
}

/// The master loads the schema before the fork. `main` returns the error, so the process exits 1.
#[test]
fn grpc_boot_fails_before_the_fork() {
    struct Case {
        name: &'static str,
        descriptor_set: &'static str,
        service: &'static str,
        log: &'static str,
    }
    let cases = [
        Case {
            name: "unknown service",
            descriptor_set: "echo.binpb",
            service: "rapira.test.v1.Missing",
            log: "grpc.services entry `rapira.test.v1.Missing` is not in",
        },
        Case {
            name: "unreadable descriptor set",
            descriptor_set: "missing.binpb",
            service: ECHO_SERVICE,
            log: "reading grpc.descriptor_set",
        },
    ];
    for case in cases {
        let (status, log) = spawn_grpc_boot_failure(case.descriptor_set, case.service);
        assert_eq!(status.code(), Some(1), "{}: {log}", case.name);
        assert!(log.contains(case.log), "{}: {log}", case.name);
    }
}

/// SIGQUIT is a graceful stop: the worker finishes the call it holds, then the master exits clean.
#[test]
fn sigquit_drains_an_in_flight_grpc_call() {
    let mut srv = spawn_grpc(1);
    let addr = srv.addr;
    let call = std::thread::spawn(move || connect_json(addr, ECHO, r#"{"text":"slow-ok"}"#));
    assert!(
        wait_log_contains(&srv, "slow started", BOOT),
        "\n{}",
        diagnostics(&srv)
    );
    signal(srv.pid(), libc::SIGQUIT);

    let got = call.join().expect("the client thread");
    assert!(
        matches!(&got, Ok((200, reply)) if reply == r#"{"text":"slow-ok"}"#),
        "{got:?}\n{}",
        diagnostics(&srv)
    );
    let status = srv.wait_exit(STOP_BUDGET);
    assert_exit_code(status, MASTER_EXIT_OK, &srv);
}
