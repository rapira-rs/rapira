//! Listener addresses, socket files, queued connections and signals during boot and drain.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use rapira_config::Mode;
use rapira_net::ListenAddr;
use tests::{StatusBody, poll};

use crate::harness::{
    BOOT, EXIT_OK, Exited, Server, Spawn, assert_exit_code, diagnostics, fixture_path, free_port,
    http_get, is_zts, parse_status_and_body, scratch_dir, signal, wait_log_contains, wait_serving,
};

const REQ: Duration = Duration::from_secs(10);
const ECHO: &str = "shared/echo-worker.php";
const GRPC_ECHO: &str = "grpc/echo-worker.php";

/// PHP sees the bound TCP port or port 80 on a Unix socket. The server logs the bound address once.
#[test]
fn the_default_server_port_is_the_bound_port() {
    struct Case {
        name: &'static str,
        mode: Mode,
        fixture: &'static str,
        path: &'static str,
        listen: ListenAddr,
        /// The text before the port in the body line that names it.
        before_port: &'static str,
        /// The port that PHP sees, or None for the port that the kernel picked.
        port: Option<u16>,
    }
    let dir = scratch_dir();
    let port_zero = ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 0)));
    let cases = [
        Case {
            name: "port 0 dispatcher serverPort()",
            mode: Mode::Dispatcher,
            fixture: "dispatcher/verbs-worker.php",
            path: "/?probe=server",
            listen: port_zero.clone(),
            before_port: "name=localhost port=",
            port: None,
        },
        Case {
            name: "port 0 worker SERVER_PORT",
            mode: Mode::Worker,
            fixture: "shared/server-variables.php",
            path: "/",
            listen: port_zero,
            before_port: "[SERVER_PORT] => ",
            port: None,
        },
        Case {
            name: "unix dispatcher serverPort()",
            mode: Mode::Dispatcher,
            fixture: "dispatcher/verbs-worker.php",
            path: "/?probe=server",
            listen: ListenAddr::Unix(dir.join("http.sock")),
            before_port: "name=localhost port=",
            port: Some(80),
        },
    ];
    for case in cases {
        let workers = if is_zts() { 2 } else { 1 };
        let mut srv = Spawn::http(case.mode, tests::fixture(case.fixture))
            .http_listen(case.listen.clone())
            .workers(workers)
            .json_log()
            .spawn();
        wait_serving(&srv, "http", workers);
        let (picked, response) = match &case.listen {
            ListenAddr::Tcp(_) => (Some(srv.addr), http_get(srv.addr, case.path, REQ)),
            ListenAddr::Unix(sock) => (None, unix_get(sock, case.path)),
        };
        let StatusBody { status, body } = response.unwrap_or_else(|e| {
            panic!(
                "{}: GET {}: {e}\n{}",
                case.name,
                case.path,
                diagnostics(&srv)
            )
        });
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, 200, "{}: {body}\n{}", case.name, diagnostics(&srv));
        let port = case.port.or(picked.map(|addr| addr.port()));
        let want = format!(
            "{}{}",
            case.before_port,
            port.expect("a case with no port listens on TCP")
        );
        assert!(
            body.lines().any(|line| line.trim() == want),
            "{}: no line {want:?} in\n{body}",
            case.name
        );

        srv.stop();
        let bound = picked.map(ListenAddr::Tcp).unwrap_or(case.listen);
        assert_one_listener(&srv, "http", &bound);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grpc_logs_its_bound_port_once() {
    let workers = if is_zts() { 2 } else { 1 };
    let mut srv = Spawn::grpc(fixture_path(GRPC_ECHO))
        .grpc_listen(ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 0))))
        .workers(workers)
        .json_log()
        .spawn();
    wait_serving(&srv, "grpc", workers);
    srv.stop();
    assert_one_listener(&srv, "grpc", &ListenAddr::Tcp(srv.addr));
}

fn assert_one_listener(srv: &Server, pool: &str, bound: &ListenAddr) {
    let records: Vec<_> = tests::server_log::records(&srv.log_file())
        .into_iter()
        .filter(|record| record.target == pool && record.message.contains("listen"))
        .map(|record| record.message)
        .collect();
    let [record] = records.as_slice() else {
        panic!(
            "expected one {pool} listener record, got {records:?}\n{}",
            diagnostics(srv)
        );
    };
    assert_eq!(record, &format!("listening on {bound}"));
}

/// A child that PHP starts inherits no socket. The fixture shell supplies file descriptors 0 to 2.
#[cfg(target_os = "linux")]
#[test]
fn php_child_processes_inherit_no_socket() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("listeners/fds-worker.php")).spawn();

    let StatusBody { status: code, body } = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body);
    assert_eq!(code, 200, "{body}\n{}", diagnostics(&srv));
    assert!(body.contains(" 0 -> "), "no fd listing: {body}");
    let inherited: Vec<&str> = body
        .lines()
        .filter(|line| line.contains("socket:"))
        .filter(|line| {
            let fd = line
                .split(" -> ")
                .next()
                .and_then(|l| l.split_whitespace().last());
            fd.and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n > 2)
        })
        .collect();
    assert!(
        inherited.is_empty(),
        "the child holds {inherited:?}\n{body}"
    );
}

/// A second server refuses a live Unix socket. A new server can use the same path after a stop.
#[test]
fn unix_socket_file_mode_live_refusal_and_stale_reclaim() {
    let dir = scratch_dir();
    let sock = dir.join("http.sock");
    let spawn = || {
        Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
            .http_listen(ListenAddr::Unix(sock.clone()))
    };

    let mut live = spawn().spawn();
    wait_serving(&live, "http", 1);
    assert_eq!(mode(&sock), 0o666);

    let Exited { status, log } = spawn().boot_failure();
    assert!(!status.success(), "{status:?}\n{log}");
    assert!(log.contains("already listening"), "{log}");
    assert_eq!(
        unix_get(&sock, "/").expect("GET / over the socket").status,
        200,
        "\n{}",
        diagnostics(&live)
    );

    live.stop();
    assert!(sock.exists(), "a stopped server leaves its socket file");

    let next = spawn().spawn();
    assert_eq!(mode(&sock), 0o666);
    assert_eq!(
        unix_get(&sock, "/").expect("GET / over the socket").status,
        200,
        "\n{}",
        diagnostics(&next)
    );
    drop(next);
    let _ = std::fs::remove_dir_all(&dir);
}

/// One boot binds every pool, so two pools on one address fail the boot at the second pool. TCP sets SO_REUSEADDR only, so the second bind gets EADDRINUSE.
#[test]
fn two_pools_on_one_address_fail_the_boot() {
    struct Case {
        name: &'static str,
        listen: ListenAddr,
        want: String,
    }

    let dir = scratch_dir();
    let tcp = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let cases = [
        Case {
            name: "tcp",
            listen: ListenAddr::Tcp(tcp),
            want: format!("bind {tcp}"),
        },
        Case {
            name: "unix",
            listen: ListenAddr::Unix(dir.join("pool.sock")),
            want: "already listening".to_owned(),
        },
    ];

    for case in &cases {
        let Exited { status, log } = Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
            .http_listen(case.listen.clone())
            .with_grpc(fixture_path(GRPC_ECHO))
            .grpc_listen(case.listen.clone())
            .boot_failure();
        assert!(!status.success(), "{}: {status:?}\n{log}", case.name);
        assert!(
            log.contains("plugin grpc: prepare failed") && log.contains(&case.want),
            "{}: no {:?}\n{log}",
            case.name,
            case.want
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each queued TCP connection gets a response without a later connection to wake the accept loop.
#[test]
fn queued_connections_survive_each_accept() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(ECHO)).spawn();
    wait_serving(&srv, "http", 1);

    let mut clients: Vec<TcpStream> = (0..16)
        .map(|_| TcpStream::connect_timeout(&srv.addr, REQ).expect("connect"))
        .collect();
    for (i, client) in clients.iter_mut().enumerate() {
        client.set_read_timeout(Some(REQ)).expect("read timeout");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
            .unwrap_or_else(|e| panic!("request {i}: {e}"));
    }
    for (i, client) in clients.iter_mut().enumerate() {
        let mut raw = String::new();
        client.read_to_string(&mut raw).unwrap_or_else(|e| {
            panic!("connection {i} got no response: {e}\n{}", diagnostics(&srv))
        });
        assert!(raw.starts_with("HTTP/1.1 200 "), "connection {i}: {raw:?}");
    }
}

/// The preload script holds PHP in MINIT while the test sends the three host signals.
#[test]
fn signals_during_the_boot_reach_the_server() {
    let preload = fixture_path("listeners/preload.php");
    let ini = tests::fixture("ini/shared/php.ini");
    let mut ini = std::fs::read_to_string(&ini)
        .unwrap_or_else(|e| panic!("read {}: {e}", ini.display()))
        + &format!(
            "\nopcache.enable = 1\nopcache.preload = {}\n",
            preload.display()
        );
    // PHP requires a preload user when the server runs as root. https://www.php.net/manual/en/opcache.configuration.php#ini.opcache.preload-user
    if unsafe { libc::geteuid() } == 0 {
        ini.push_str("opcache.preload_user = root\n");
    }

    let srv = Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
        .php_ini(&ini)
        .spawn();
    let marker = srv.dir.join("preloading");
    poll(BOOT, || marker.is_file().then_some(()))
        .unwrap_or_else(|| panic!("PHP did not enter preload\n{}", diagnostics(&srv)));
    signal(srv.pid(), libc::SIGHUP);
    signal(srv.pid(), libc::SIGUSR1);
    signal(srv.pid(), libc::SIGUSR2);
    std::fs::write(srv.dir.join("release-preload"), "").expect("release MINIT");

    // Only the USR1 sent during MINIT can produce this record.
    assert!(
        wait_log_contains(&srv, "pool=http worker=0 ", BOOT),
        "no status after MINIT\n{}",
        diagnostics(&srv)
    );
    poll(BOOT, || {
        let log = std::fs::read_to_string(srv.log_file()).expect("read server.log");
        (log.matches("reloading all pools").count() == 2).then_some(())
    })
    .unwrap_or_else(|| panic!("expected both reload signals\n{}", diagnostics(&srv)));
    let code = http_get(srv.addr, "/", REQ).expect("GET /").status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

/// The server closes the Unix listener while a request is in flight. The held request still gets its response.
#[test]
fn a_stop_refuses_new_connections_during_the_drain() {
    let dir = scratch_dir();
    let sock = dir.join("http.sock");
    let release = dir.join("release");
    let mut srv = Spawn::http(Mode::Worker, tests::fixture("worker/held-worker.php"))
        .http_listen(ListenAddr::Unix(sock.clone()))
        .json_log()
        .spawn();
    let mut held = UnixStream::connect(&sock).expect("connect");
    held.set_read_timeout(Some(REQ)).expect("read timeout");
    write!(held, "GET /?release={} HTTP/1.0\r\n\r\n", release.display())
        .expect("write the request");
    tests::server_log::wait_app_record(&srv.log_file(), "held");

    signal(srv.pid(), libc::SIGTERM);
    let refused = poll(Duration::from_secs(5), || {
        UnixStream::connect(&sock)
            .err()
            .filter(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
    });
    std::fs::write(&release, "").expect("write the release file");
    assert!(
        refused.is_some(),
        "a new connection waits in the backlog during the drain\n{}",
        diagnostics(&srv)
    );
    let mut raw = Vec::new();
    held.read_to_end(&mut raw).expect("read the held response");
    let StatusBody { status: code, body } = parse_status_and_body(&raw)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&raw)));
    assert_eq!((code, body), (200, &b"done"[..]), "\n{}", diagnostics(&srv));
    assert_exit_code(srv.wait_exit(REQ), EXIT_OK, &srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The permission bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// One HTTP/1.0 GET of `target` over the unix socket `sock`.
fn unix_get(sock: &Path, target: &str) -> std::io::Result<StatusBody<Vec<u8>>> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(REQ))?;
    write!(stream, "GET {target} HTTP/1.0\r\n\r\n")?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_status_and_body(&raw).map(|StatusBody { status, body }| StatusBody {
        status,
        body: body.to_vec(),
    })
}
