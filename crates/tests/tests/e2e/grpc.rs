use std::net::SocketAddr;

use http::Method;
use rapira_net::ListenAddr;
use serde_json::Value;
use tests::grpc::{Conn, ECHO_PATH as ECHO, Fields, Wire, envelope, fields};
use tests::{StatusBody, server_log};

use crate::harness::{
    BOOT, EXIT_OK, STOP_BUDGET, Spawn, assert_exit_code, diagnostics, fixture_path, signal,
    wait_log_contains, wait_serving,
};

/// Encodes `EchoRequest { text }` as field 1, length-delimited, for a text shorter than 128 bytes.
fn echo_request(text: &str) -> Vec<u8> {
    let mut message = vec![0x0a, text.len() as u8];
    message.extend_from_slice(text.as_bytes());
    message
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("client runtime")
}

/// The two paths that only real PHP behind the real listener proves: the deadline reaches PHP as a cancel, and metadata passes between the client and PHP in both directions. Sources: the contract gRPC README (the host enforces `Call\Context::$deadline`), RFC 4648: 00 ff is `AP8`, and the contract `Grpc\Metadata` with the PHP key casts (https://www.php.net/manual/en/language.types.array.php#language.types.array.key-casts): a canonical decimal key is an int, and `01` stays a string.
#[test]
fn php_outcomes_reach_the_client() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        text: &'static str,
        metadata: Fields,
        grpc_status: &'static str,
        headers: Fields,
        trailers: Fields,
        /// The message of an app record that PHP leaves.
        log_message: &'static str,
        /// The context of that record, as JSON.
        log_context: &'static str,
    }
    // The metadata case has no deadline, so it waits until the worker boot ends, and PHP is in its loop when the deadline case starts.
    let cases = [
        Case {
            name: "metadata crosses the edge",
            text: "meta",
            metadata: &[
                ("x-echo", "a"),
                ("x-echo-bin", "AP8"),
                ("123", "n"),
                ("-1", "n"),
                ("0", "n"),
                ("01", "n"),
            ],
            grpc_status: "0",
            headers: &[("x-echo", "a")],
            trailers: &[("x-echo-bin", "AP8")],
            log_message: "meta",
            log_context: r#"{"keys":["x-echo","x-echo-bin",123,-1,0,"01"]}"#,
        },
        Case {
            name: "the deadline cancels the php call",
            text: "slow",
            metadata: &[("grpc-timeout", "300m")],
            grpc_status: "4",
            headers: &[],
            trailers: &[],
            log_message: "slow",
            log_context: r#"{"cancelled":"yes","respond":"Rapira\\Exception\\WorkDiscardedException"}"#,
        },
    ];

    let mut srv = Spawn::grpc(fixture_path("grpc/echo-worker.php"))
        .json_log()
        .spawn();
    let listen = ListenAddr::Tcp(srv.addr);
    let rt = runtime();
    for case in cases {
        let got = rt.block_on(async {
            let mut conn = Conn::open(&listen, Wire::H2).await?;
            conn.grpc(ECHO, case.metadata, &envelope(&echo_request(case.text)))
                .await
        })?;
        assert_eq!(
            got.grpc_status(),
            Some(case.grpc_status),
            "{}: {got:?}",
            case.name
        );
        for (name, want) in fields(case.headers).iter() {
            assert_eq!(
                got.headers.get(name),
                Some(want),
                "{}: header {name}: {got:?}",
                case.name
            );
        }
        for (name, want) in fields(case.trailers).iter() {
            assert_eq!(
                got.trailers.get(name),
                Some(want),
                "{}: trailer {name}: {got:?}",
                case.name
            );
        }
        let got: Value = serde_json::from_str(&server_log::wait_app_record(
            &srv.log_file(),
            case.log_message,
        ))?;
        let want: Value = serde_json::from_str(case.log_context)?;
        assert_eq!(got, want, "{}: {} record", case.name, case.log_message);
    }
    // The client runtime holds the client connections. Dropping it closes them, so the drain does not wait on them.
    drop(rt);
    srv.stop();
    Ok(())
}

/// A Connect unary call with a JSON body over HTTP/1.1. The server may send the reply chunked, so hyper reads it.
fn connect_json(addr: SocketAddr, path: &str, body: &str) -> anyhow::Result<StatusBody<String>> {
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
        anyhow::Ok(StatusBody {
            status: got.status,
            body: String::from_utf8_lossy(&got.body).into_owned(),
        })
    };
    runtime().block_on(async { tokio::time::timeout(BOOT, call).await })?
}

/// Sources: the Connect protocol (a unary call posts the message as proto3 JSON), `grpc/health/v1/health.proto`, and `fixtures/grpc/echo.proto`. With no `services` key, the pool serves both services of the file that no other file imports: `EchoService` with five methods and `OtherService` with one.
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
            name: "the plugin answers health",
            path: "/grpc.health.v1.Health/Check",
            body: "{}",
            reply: r#"{"status":"SERVING"}"#,
        },
        Case {
            name: "the dispatcher reads the configured services",
            path: ECHO,
            body: r#"{"text":"services"}"#,
            reply: r#"{"text":"rapira.test.v1.EchoService:5,rapira.test.v1.OtherService:1"}"#,
        },
    ];
    let srv = Spawn::grpc(fixture_path("grpc/echo-worker.php"))
        .services(None)
        .spawn();
    wait_serving(&srv, "grpc", 1);
    for case in cases {
        let got = connect_json(srv.addr, case.path, case.body);
        assert!(
            matches!(&got, Ok(StatusBody { status: 200, body }) if body == case.reply),
            "{}: {got:?}\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// SIGQUIT starts a drain. The worker finishes the call before the server exits.
#[test]
fn sigquit_drains_an_in_flight_grpc_call() {
    let mut srv = Spawn::grpc(fixture_path("grpc/echo-worker.php")).spawn();
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
        matches!(&got, Ok(StatusBody { status: 200, body }) if body == r#"{"text":"slow-ok"}"#),
        "{got:?}\n{}",
        diagnostics(&srv)
    );
    let status = srv.wait_exit(STOP_BUDGET);
    assert_exit_code(status, EXIT_OK, &srv);
}
