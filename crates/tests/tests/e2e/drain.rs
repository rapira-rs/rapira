use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::net::TcpStream;
use std::time::Duration;

const READ: Duration = Duration::from_secs(10);
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n";

/// Send the request bytes and check the status, Connection header and body.
fn exchange(conn: &mut Conn, bytes: &[u8], connection: Option<&str>) -> std::io::Result<()> {
    conn.send(bytes)?;
    let head = conn.read_head(READ)?;
    assert_eq!(head.status, 200);
    assert_eq!(field(&head, "connection"), connection);
    let length = field(&head, "content-length")
        .and_then(|v| v.parse().ok())
        .expect("the response declares its length");
    assert_eq!(conn.read_n(length, READ)?, b"ok");
    Ok(())
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
            exchange(&mut conn, REQUEST, None).expect("the first exchange");
            conn
        })
        .collect();
    let mut fresh = Conn::open(srv.addr, READ).expect("connect before the stop");
    // The 100 response on the next connection confirms that the listener accepted both. The POST keeps the drain open until we send its body.
    let mut held = Conn::open(srv.addr, READ).expect("connect for the held POST");
    held.send(b"POST / HTTP/1.1\r\nHost: e2e\r\nExpect: 100-continue\r\nContent-Length: 1\r\n\r\n")
        .expect("send the POST headers");
    assert_eq!(held.read_head(READ).expect("interim response").status, 100);
    signal(srv.pid(), libc::SIGQUIT);
    assert!(
        wait_log_contains(&srv, "draining 6 open connections", READ),
        "no drain record\n{}",
        diagnostics(&srv)
    );
    assert_new_connection_refused(&srv);
    for (i, conn) in used.iter_mut().enumerate() {
        exchange(conn, REQUEST, Some("close"))
            .unwrap_or_else(|e| panic!("held connection {i}: {e}\n{}", diagnostics(&srv)));
    }
    exchange(&mut fresh, REQUEST, Some("close")).expect("the first request during the drain");
    exchange(&mut held, b"x", Some("close")).expect("complete the held POST");
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
