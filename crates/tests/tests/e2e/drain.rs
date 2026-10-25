use crate::harness::*;
use std::time::Duration;

const READ: Duration = Duration::from_secs(10);
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: e2e\r\n\r\n";

/// One keep-alive exchange on `conn`: the response head, then its body by content-length.
fn exchange(conn: &mut Conn) -> std::io::Result<Head> {
    conn.send(REQUEST)?;
    let head = conn.read_head(READ)?;
    let length = field(&head, "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    conn.read_n(length, READ)?;
    Ok(head)
}

fn field<'a>(head: &'a Head, name: &str) -> Option<&'a str> {
    head.fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// A stopping worker keeps the connections it holds: a request on each gets a complete response with `connection: close`, and the worker exits once they are closed.
#[test]
fn a_stopping_worker_answers_requests_on_the_connections_it_holds() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "");
    let pid = wait_workers(&srv, READ, "1 worker", |p| p.len() == 1)[0];
    let mut used: Vec<Conn> = (0..4)
        .map(|_| {
            let mut conn = Conn::open(srv.addr, READ).expect("connect");
            let head = exchange(&mut conn).expect("the first exchange");
            assert_eq!(head.status, 200, "\n{}", diagnostics(&srv));
            assert_eq!(field(&head, "connection"), None, "a kept-alive response");
            conn
        })
        .collect();
    let mut fresh: Vec<Conn> = (0..4)
        .map(|_| Conn::open(srv.addr, READ).expect("connect"))
        .collect();

    signal(pid, libc::SIGQUIT);
    assert!(
        wait_log_contains(&srv, "open connections", READ),
        "no drain record\n{}",
        diagnostics(&srv)
    );

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
    // The old worker or its replacement answers, depending on which one accepted the connection.
    for (i, conn) in fresh.iter_mut().enumerate() {
        let head = exchange(conn)
            .unwrap_or_else(|e| panic!("new connection {i}: {e}\n{}", diagnostics(&srv)));
        assert_eq!(head.status, 200, "new connection {i}");
    }
    // Well under the 25 s drain grace: the drain ends when the last connection closes.
    wait_workers(&srv, READ, "a new worker after the drain", |p| {
        p.len() == 1 && p[0] != pid
    });
}

/// One Connect unary call to EchoService/Echo on `conn`.
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

/// A stopping gRPC worker answers the next Connect call on each HTTP/1.1 connection it holds, and that response closes the connection.
#[tokio::test]
async fn a_stopping_grpc_worker_answers_on_its_http1_connections() {
    let srv = Spawn::grpc(fixture_path("grpc/echo-worker.php"))
        .json_log()
        .spawn();
    let pid = wait_workers(&srv, READ, "1 worker", |p| p.len() == 1)[0];
    let mut conns = Vec::new();
    for _ in 0..4 {
        let mut conn = tests::grpc::Conn::open(&listen(&srv), tests::grpc::Wire::Http1)
            .await
            .expect("connect");
        let response = connect_call(&mut conn).await;
        assert_eq!(response.status, 200, "\n{}", diagnostics(&srv));
        conns.push(conn);
    }

    signal(pid, libc::SIGQUIT);
    assert!(
        wait_log_contains(&srv, "draining open connections", READ),
        "no drain record\n{}",
        diagnostics(&srv)
    );

    for (i, conn) in conns.iter_mut().enumerate() {
        let response = connect_call(conn).await;
        assert_eq!(
            response.status,
            200,
            "connection {i}\n{}",
            diagnostics(&srv)
        );
        assert_eq!(
            response
                .headers
                .get(http::header::CONNECTION)
                .and_then(|v| v.to_str().ok()),
            Some("close"),
            "connection {i}"
        );
    }
}
