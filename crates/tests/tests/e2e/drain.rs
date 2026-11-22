use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::net::TcpStream;
use std::time::Duration;

const READ: Duration = Duration::from_secs(10);
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n";

/// Runs one keep-alive exchange on `conn` and returns the response head. The body is read by content-length.
fn exchange(conn: &mut Conn) -> std::io::Result<Head> {
    conn.send(REQUEST)?;
    let head = conn.read_head(READ)?;
    let length = field(&head, "content-length")
        .and_then(|v| v.parse().ok())
        .expect("the response declares its length");
    assert_eq!(conn.read_n(length, READ)?, b"ok");
    Ok(head)
}

fn field<'a>(head: &'a Head, name: &str) -> Option<&'a str> {
    head.fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn assert_new_connection_refused(srv: &Server) {
    tests::poll(READ, || {
        TcpStream::connect_timeout(&srv.addr, READ)
            .err()
            .filter(|err| err.kind() == std::io::ErrorKind::ConnectionRefused)
    })
    .unwrap_or_else(|| panic!("new connections still get through\n{}", diagnostics(srv)));
}

/// During a stop, each accepted HTTP connection gets its last response with `connection: close`.
#[test]
fn a_stopping_server_answers_requests_on_its_http_connections() {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/boot-log-worker.php"),
    )
    .spawn();
    wait_serving(&srv, "http", 1);
    let mut used: Vec<Conn> = (0..4)
        .map(|_| {
            let mut conn = Conn::open(srv.addr, READ).expect("connect");
            let head = exchange(&mut conn).expect("the first exchange");
            assert_eq!(head.status, 200, "\n{}", diagnostics(&srv));
            assert_eq!(field(&head, "connection"), None, "a kept-alive response");
            conn
        })
        .collect();
    signal(srv.pid(), libc::SIGQUIT);
    assert!(
        wait_log_contains(&srv, "open connections", READ),
        "no drain record\n{}",
        diagnostics(&srv)
    );
    assert_new_connection_refused(&srv);
    for (i, conn) in used.iter_mut().enumerate() {
        let head = exchange(conn)
            .unwrap_or_else(|e| panic!("held connection {i}: {e}\n{}", diagnostics(&srv)));
        assert_eq!(head.status, 200, "held connection {i}");
        assert_eq!(
            field(&head, "connection"),
            Some("close"),
            "held connection {i}"
        );
    }
    assert_exit_code(srv.wait_exit(READ), EXIT_OK, &srv);
}

/// Sends one Connect unary call to EchoService/Echo on `conn`.
async fn connect_call(conn: &mut tests::grpc::Conn) -> tests::grpc::Response {
    let exchange = conn.send(
        http::Method::POST,
        tests::grpc::ECHO_PATH,
        &[
            ("content-type", "application/proto"),
            ("connect-protocol-version", "1"),
        ],
        tests::grpc::HI,
    );
    tokio::time::timeout(READ, exchange)
        .await
        .expect("no response within 10 s")
        .expect("the call")
}

/// During a stop, each accepted gRPC HTTP/1.1 connection gets its last Connect response.
#[tokio::test]
async fn a_stopping_server_answers_on_its_grpc_http1_connections() {
    let srv = Spawn::grpc(fixture_path("grpc/echo-worker.php"))
        .json_log()
        .spawn();
    wait_serving(&srv, "grpc", 1);
    let mut conns = Vec::new();
    for _ in 0..4 {
        let mut conn = tests::grpc::Conn::open(&listen(&srv), tests::grpc::Wire::Http1)
            .await
            .expect("connect");
        let response = connect_call(&mut conn).await;
        assert_eq!(response.status, 200, "\n{}", diagnostics(&srv));
        conns.push(conn);
    }

    signal(srv.pid(), libc::SIGQUIT);
    assert!(
        wait_log_contains(&srv, "draining open connections", READ),
        "no drain record\n{}",
        diagnostics(&srv)
    );
    assert_new_connection_refused(&srv);

    for (i, conn) in conns.iter_mut().enumerate() {
        let response = connect_call(conn).await;
        assert_eq!(
            response.status,
            200,
            "connection {i}\n{}",
            diagnostics(&srv)
        );
        assert_eq!(response.body.as_ref(), tests::grpc::HI, "connection {i}");
        assert_eq!(
            response
                .headers
                .get(http::header::CONNECTION)
                .and_then(|v| v.to_str().ok()),
            Some("close"),
            "connection {i}"
        );
    }
    tokio::time::timeout(READ, exited(srv))
        .await
        .expect("the server exits after the last response");
}
