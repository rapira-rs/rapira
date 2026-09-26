//! The listeners that the master binds before the fork: the bound address, the unix socket file, one address per boot, and the accept loop of each worker.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use rapira_net::ListenAddr;
use rapira_sapi::Mode;

use crate::harness::{
    BOOT, Server, Spawn, diagnostics, fixture_path, free_port, http_get, listen, scratch_dir,
    spawn_with_config, wait_workers, worker_pids,
};

const REQ: Duration = Duration::from_secs(10);
const ECHO: &str = "shared/echo-worker.php";
const GRPC_ECHO: &str = "grpc/echo-worker.php";

/// Port 0 lets the kernel pick the port. The log names the port that the listener got, and the server answers there. The spawn waits for the `[grpc]` pool.
#[test]
fn port_zero_serves_on_the_logged_port() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
        .http_listen(ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 0))))
        .with_grpc(fixture_path(GRPC_ECHO))
        .spawn();
    let addr = logged_http_addr(&srv);
    assert_ne!(addr.port(), 0, "\n{}", diagnostics(&srv));

    let (code, body) = http_get(addr, "/", REQ).expect("GET / on the logged port");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(
        String::from_utf8_lossy(&body).starts_with("ok:"),
        "{body:?}"
    );
}

/// The listener is close-on-exec, so a child process that PHP starts holds no socket of the worker. Fds 0 to 2 come from the shell of the fixture.
#[cfg(target_os = "linux")]
#[test]
fn php_child_processes_inherit_no_socket() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("listeners/fds-worker.php")).spawn();

    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
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

/// The socket file is world-connectable. A second master refuses a path that a live master listens on and leaves that socket alone. The file that a stopped master leaves behind does not block the next boot.
#[test]
fn unix_socket_file_mode_live_refusal_and_stale_reclaim() {
    let dir = scratch_dir();
    let sock = dir.join("http.sock");
    let spawn = || Spawn::http(Mode::Dispatcher, fixture_path(ECHO)).http_unix(&sock);

    let mut live = spawn().spawn();
    assert_eq!(mode(&sock), 0o666);

    let (status, log) = spawn().boot_failure();
    assert!(!status.success(), "{status:?}\n{log}");
    assert!(log.contains("already listening"), "{log}");
    assert_eq!(unix_status(&sock), 200, "\n{}", diagnostics(&live));

    live.stop();
    assert!(sock.exists(), "a stopped master leaves its socket file");

    let next = spawn().spawn();
    assert_eq!(mode(&sock), 0o666);
    assert_eq!(unix_status(&sock), 200, "\n{}", diagnostics(&next));
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
        let (status, log) = Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
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

/// Each pool gets only the listeners that it bound. An ondemand pool forks on a connection to its own listener: a connection to the `[http]` listener forks no `[grpc]` worker.
#[test]
fn an_ondemand_pool_forks_only_for_its_own_listener() {
    let srv = Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
        .http_pool("scaling = \"ondemand\"")
        .with_grpc(fixture_path(GRPC_ECHO))
        .grpc_pool("scaling = \"ondemand\"")
        .spawn();

    // The master polls every listener of an idle ondemand pool, and one poll result forks every pool that owns a readable listener. So when the response arrives, a wrong listener list has already forked a [grpc] worker.
    let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        worker_pids(srv.pid()).len(),
        1,
        "a connection to [http] must fork the [http] worker only\n{}",
        diagnostics(&srv)
    );

    let ListenAddr::Tcp(grpc) = listen(&srv) else {
        panic!("the [grpc] pool listens on TCP");
    };
    let _client = TcpStream::connect_timeout(&grpc, REQ).expect("connect to [grpc]");
    wait_workers(&srv, Duration::from_secs(20), "the [grpc] worker", |p| {
        p.len() == 2
    });
}

/// One worker takes every connection. The connections after the first wait in the backlog while the worker moves its listener entry to the tail of the wait queue after each accept, and no new connection arrives to wake it again.
#[test]
fn queued_connections_survive_each_accept() {
    let srv = spawn_with_config(ECHO, 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

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

/// Signals that terminate by default stay pending while PHP starts, so a HUP, USR1 or USR2 during the boot does not stop the master. An OPcache preload script sends them to the master from inside the boot, before the master installs its handlers.
#[test]
fn signals_during_the_boot_do_not_stop_the_master() {
    let dir = scratch_dir();
    let marker = dir.join("preload.out");
    let preload = dir.join("preload.php");
    std::fs::write(
        &preload,
        format!(
            "<?php\nif (!function_exists('posix_kill')) {{\n    file_put_contents('{marker}', 'skip');\n    return;\n}}\n\
             foreach ([{hup}, {usr1}, {usr2}] as $signal) {{\n    posix_kill(posix_getpid(), $signal);\n}}\n\
             file_put_contents('{marker}', 'sent');\n",
            marker = marker.display(),
            hup = libc::SIGHUP,
            usr1 = libc::SIGUSR1,
            usr2 = libc::SIGUSR2,
        ),
    )
    .expect("write preload.php");
    let ini = tests::fixture("ini/shared/php.ini");
    let ini = std::fs::read_to_string(&ini)
        .unwrap_or_else(|e| panic!("read {}: {e}", ini.display()))
        + &format!("\nopcache.preload = {}\n", preload.display());

    let mut srv = Spawn::http(Mode::Dispatcher, fixture_path(ECHO))
        .php_ini(&ini)
        .spawn();
    let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(
        srv.child.try_wait().ok().flatten().is_none(),
        "\n{}",
        diagnostics(&srv)
    );

    match std::fs::read_to_string(&marker).as_deref() {
        Ok("sent") => {}
        Ok("skip") => tests::assert_skip_allowed("posix"),
        // The preload script never ran: OPcache is not loaded.
        _ => tests::assert_skip_allowed("opcache"),
    }
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The address of the `[http]` listener, from its `prepared listener on` record in the log of `srv`.
fn logged_http_addr(srv: &Server) -> SocketAddr {
    const RECORD: &str = "http: prepared listener on ";
    let end = Instant::now() + BOOT;
    loop {
        let log = std::fs::read_to_string(srv.log_file()).unwrap_or_default();
        if let Some(line) = log.lines().find(|line| line.contains(RECORD)) {
            let (_, addr) = line.split_once(RECORD).expect("the record");
            return addr
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("address {addr:?}: {e}\n{log}"));
        }
        assert!(
            Instant::now() < end,
            "no {RECORD:?} record\n{}",
            diagnostics(srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The permission bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// The status code of one HTTP/1.0 `GET /` over the unix socket `sock`.
fn unix_status(sock: &Path) -> u16 {
    let mut stream =
        UnixStream::connect(sock).unwrap_or_else(|e| panic!("connect {}: {e}", sock.display()));
    stream.set_read_timeout(Some(REQ)).expect("read timeout");
    stream
        .write_all(b"GET / HTTP/1.0\r\n\r\n")
        .expect("write the request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read the response");
    raw.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {raw:?}"))
}
