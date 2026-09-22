use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Connect budget for a freshly spawned master (CI macOS worst case).
pub const BOOT: Duration = Duration::from_secs(30);

/// Master could not bring up a serviceable gen-0 pool.
pub const MASTER_EXIT_FAILBOOT: i32 = 70;
pub const MASTER_EXIT_OK: i32 = 0;
/// Master forced stop (a second signal arrived while draining).
pub const MASTER_EXIT_FORCED: i32 = 130;

/// A running master and the scratch dir holding its config and log.
pub struct Server {
    pub child: Child,
    pub addr: SocketAddr,
    pub dir: PathBuf,
}

impl Server {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let end = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(st)) => return Some(st),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn try_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for Server {
    /// Signals and snapshots only while the master is unreaped: a reaped pid can be reused.
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            signal(self.child.id(), libc::SIGTERM);
            if self.wait_exit(Duration::from_secs(5)).is_none() {
                let kids = worker_pids(self.child.id());
                signal(self.child.id(), libc::SIGKILL);
                for k in kids {
                    signal(k, libc::SIGKILL);
                }
                let _ = self.child.wait();
            }
        }
        if std::thread::panicking() {
            eprintln!("{}", log_tail(&self.dir));
            return;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The bin belongs to the root package, so CARGO_BIN_EXE is unset here: locate it beside the test binary, or via `RAPIRA_BIN`.
fn rapira_bin() -> PathBuf {
    if let Ok(p) = std::env::var("RAPIRA_BIN") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().expect("current_exe");
    let bin = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile> dir")
        .join("rapira");
    assert!(
        bin.exists(),
        "rapira binary not found at {}; build it first (cargo build -p rapira_core --bin rapira) or set RAPIRA_BIN",
        bin.display()
    );
    bin
}

/// `extra_toml` is appended inside `[http.pool]`, bare keys first; a `[log]` or `[supervisor]` header may follow, and it must not open `[http]` or `[http.*]`.
pub fn spawn_with_config(fixture: &str, processes: usize, extra_toml: &str) -> Server {
    spawn_with_extras(fixture, processes, "", extra_toml, Some("info"), None)
}

/// [`spawn_with_config`] for keys inside `[http]`: `http_extra` follows `listen` and may open `[http.static]` or `[http.uploads]`.
pub fn spawn_with_http_extra(fixture: &str, processes: usize, http_extra: &str) -> Server {
    spawn_with_extras(fixture, processes, http_extra, "", Some("info"), None)
}

/// [`spawn_with_config`] without the pinned `RUST_LOG`, so the `[log]` section owns the filter.
pub fn spawn_without_rust_log(fixture: &str, processes: usize, extra_toml: &str) -> Server {
    spawn_with_extras(fixture, processes, "", extra_toml, None, None)
}

/// A `php.ini` written into the directory the child runs from, optionally also pointed at by PHPRC.
pub struct CwdIni<'a> {
    pub contents: &'a str,
    pub via_phprc: bool,
}

/// Pins that the SAPI does not read ini files from the cwd.
pub fn spawn_in_cwd(fixture: &str, processes: usize, php_ini: &str) -> Server {
    let ini = CwdIni {
        contents: php_ini,
        via_phprc: false,
    };
    spawn_with_extras(fixture, processes, "", "", Some("info"), Some(ini))
}

/// [`spawn_in_cwd`] with PHPRC pointing at the same directory; `extra_toml` is appended inside `[http.pool]`, bare keys first; a `[log]` or `[supervisor]` header may follow, and it must not open `[http]` or `[http.*]`.
pub fn spawn_with_phprc_and_config(
    fixture: &str,
    processes: usize,
    php_ini: &str,
    extra_toml: &str,
) -> Server {
    let ini = CwdIni {
        contents: php_ini,
        via_phprc: true,
    };
    spawn_with_extras(fixture, processes, "", extra_toml, Some("info"), Some(ini))
}

fn spawn_with_extras(
    fixture: &str,
    processes: usize,
    http_extra: &str,
    extra_toml: &str,
    rust_log: Option<&str>,
    cwd_ini: Option<CwdIni<'_>>,
) -> Server {
    let (dir, entrypoint) = stage_fixture(fixture);
    if let Some(ini) = &cwd_ini {
        std::fs::write(dir.join("php.ini"), ini.contents).expect("write php.ini");
    }
    let mut last_log = String::new();
    for _ in 0..3 {
        let (mut child, addr) = spawn_attempt(
            &dir,
            processes,
            &entrypoint,
            http_extra,
            extra_toml,
            rust_log,
            cwd_ini.as_ref(),
        );
        if wait_for_port(&addr, &mut child, BOOT) {
            return Server { child, addr, dir };
        }
        let _ = child.kill();
        let _ = child.wait();
        last_log = log_tail(&dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    panic!("rapira never accepted a connection after 3 attempts\n{last_log}");
}

/// A scratch dir holding a copy of the fixture, plus the entrypoint name for the config.
fn stage_fixture(fixture: &str) -> (PathBuf, String) {
    let dir = scratch_dir();
    let name = Path::new(fixture)
        .file_name()
        .unwrap_or_else(|| panic!("fixture {fixture} has no file name"));
    std::fs::copy(fixture_path(fixture), dir.join(name))
        .unwrap_or_else(|e| panic!("copy fixture {fixture}: {e}"));
    let entrypoint = name.to_str().expect("fixture name is utf-8").to_owned();
    (dir, entrypoint)
}

/// One spawn on a fresh port; the caller decides how to wait.
fn spawn_attempt(
    dir: &Path,
    processes: usize,
    entrypoint: &str,
    http_extra: &str,
    extra_toml: &str,
    rust_log: Option<&str>,
    cwd_ini: Option<&CwdIni<'_>>,
) -> (Child, SocketAddr) {
    let port = free_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    std::fs::write(
        dir.join("rapira.toml"),
        render_config(port, processes, entrypoint, http_extra, extra_toml),
    )
    .expect("write config");
    let log = File::create(dir.join("server.log")).expect("create server.log");
    let mut cmd = Command::new(rapira_bin());
    cmd.arg("serve").arg(dir.join("rapira.toml"));
    cmd.env_remove("PHPRC");
    if let Some(ini) = cwd_ini {
        cmd.current_dir(dir);
        if ini.via_phprc {
            cmd.env("PHPRC", dir);
        }
    }
    match rust_log {
        Some(v) => cmd.env("RUST_LOG", v),
        None => cmd.env_remove("RUST_LOG"),
    };
    let child = cmd
        .stdout(Stdio::from(log.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn rapira");
    (child, addr)
}

/// Boots expecting a startup failure: waits for the exit and returns the status with the log.
/// The function returns the whole log, not the tail: with `RUST_BACKTRACE` set, the backtrace
/// after the error line is longer than the tail window.
pub fn spawn_boot_failure(fixture: &str, http_extra: &str) -> (ExitStatus, String) {
    let (dir, entrypoint) = stage_fixture(fixture);
    boot_failure(dir, &entrypoint, http_extra)
}

/// [`spawn_boot_failure`] with the entrypoint written into the config verbatim, so a path no
/// fixture can stage reaches the boot check.
pub fn spawn_boot_failure_with_entrypoint(entrypoint: &str) -> (ExitStatus, String) {
    boot_failure(scratch_dir(), entrypoint, "")
}

fn boot_failure(dir: PathBuf, entrypoint: &str, http_extra: &str) -> (ExitStatus, String) {
    let (child, addr) = spawn_attempt(&dir, 1, entrypoint, http_extra, "", Some("info"), None);
    let mut srv = Server { child, addr, dir };
    let Some(status) = srv.wait_exit(BOOT) else {
        panic!("rapira did not exit");
    };
    let log = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
    (status, log)
}

/// Renders `[http.pool]` before `[http]`.
/// TOML accepts an explicit super-table header after its sub-table.
/// `http_extra` renders last, so a header it opens ends the file.
fn render_config(
    port: u16,
    processes: usize,
    fixture: &str,
    http_extra: &str,
    extra: &str,
) -> String {
    format!(
        "[http.pool]\nprocesses = {processes}\nentrypoint = \"{fixture}\"\n{extra}\n[http]\nlisten = \"127.0.0.1:{port}\"\n{http_extra}"
    )
}

/// Connect-only readiness: the master binds the listen socket before forking, so a successful connect means it booted far enough to serve.
fn wait_for_port(addr: &SocketAddr, child: &mut Child, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if TcpStream::connect_timeout(addr, Duration::from_millis(200)).is_ok() {
            std::thread::sleep(Duration::from_millis(100));
            return child.try_wait().ok().flatten().is_none();
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// HTTP/1.1 GET with `Connection: close`: the body is close-delimited, so no chunked or keep-alive parsing is needed.
pub fn http_get(addr: SocketAddr, path: &str, timeout: Duration) -> io::Result<(u16, Vec<u8>)> {
    http_get_with_headers(addr, path, &[], timeout)
}

/// [`http_get`] plus extra request fields, written in the order given so a repeated name stays repeated on the wire.
pub fn http_get_with_headers(
    addr: SocketAddr,
    path: &str,
    fields: &[(&str, &str)],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    parse_status_and_body(&http_get_raw(addr, path, fields, timeout)?)
}

/// The whole response, head included: for assertions about which fields reached the client.
pub fn http_get_raw(
    addr: SocketAddr,
    path: &str,
    fields: &[(&str, &str)],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n"
    )?;
    for (name, value) in fields {
        write!(s, "{name}: {value}\r\n")?;
    }
    write!(s, "\r\n")?;
    s.flush()?;
    let mut raw = Vec::new();
    if let Err(e) = s.read_to_end(&mut raw)
        && !(raw.is_empty() && e.kind() == io::ErrorKind::ConnectionReset)
    {
        return Err(e);
    }
    Ok(raw)
}

/// As [`http_raw`], returning the unparsed response bytes for asserting on the header block itself.
pub fn http_raw_bytes(addr: SocketAddr, request: &[u8], timeout: Duration) -> io::Result<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    s.write_all(request)?;
    s.flush()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    Ok(raw)
}

/// A caller-controlled request: no implicit Host or Connection line.
pub fn http_raw(addr: SocketAddr, request: &[u8], timeout: Duration) -> io::Result<(u16, Vec<u8>)> {
    parse_status_and_body(&http_raw_bytes(addr, request, timeout)?)
}

/// Sibling of [`http_get`] with a body; `content_type` is bytes because a multipart boundary is opaque octets and obs-text is legal in a field value.
pub fn http_post(
    addr: SocketAddr,
    path: &str,
    content_type: &[u8],
    body: &[u8],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    let mut req = Vec::new();
    write!(
        req,
        "POST {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n"
    )?;
    req.extend_from_slice(b"Content-Type: ");
    req.extend_from_slice(content_type);
    write!(req, "\r\nContent-Length: {}\r\n\r\n", body.len())?;
    req.extend_from_slice(body);
    http_raw(addr, &req, timeout)
}

fn parse_status_and_body(raw: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    if raw.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "closed before any response byte",
        ));
    }
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let status_end = raw
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(head_end);
    let status_line = std::str::from_utf8(&raw[..status_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 status line"))?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status code"))?;
    Ok((code, raw[head_end + 4..].to_vec()))
}

/// Direct children of `master`; the `ps` field syntax is valid on both procps and BSD ps, and `Z` is excluded so a dead-but-unreaped worker is not counted.
pub fn worker_pids(master: u32) -> Vec<u32> {
    let out = match Command::new("ps")
        .args(["-axo", "pid=,ppid=,state="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pids = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(state)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if ppid == master && !state.starts_with('Z') {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// Poll `worker_pids` until `pred` holds; panic with diagnostics on deadline.
pub fn wait_workers(
    srv: &Server,
    deadline: Duration,
    what: &str,
    pred: impl Fn(&[u32]) -> bool,
) -> Vec<u32> {
    let master = srv.child.id();
    let end = Instant::now() + deadline;
    loop {
        let pids = worker_pids(master);
        if pred(&pids) {
            return pids;
        }
        if Instant::now() >= end {
            panic!(
                "timed out after {deadline:?} waiting for {what}\n{}",
                diagnostics(srv)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn signal(pid: u32, sig: i32) {
    // SAFETY: kill is a plain syscall; a stale pid returns ESRCH, which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Per-thread outcome counters: `refused` means the listener closed, `failed` means a non-200 response, a hang, or a corrupt reply; connection drops only record `last_err` (the balancer retries those).
pub struct Tally {
    pub ok: u64,
    pub refused: u64,
    pub failed: u64,
    pub last_err: Option<String>,
}

impl Tally {
    fn new() -> Tally {
        Tally {
            ok: 0,
            refused: 0,
            failed: 0,
            last_err: None,
        }
    }
}

/// A pool of threads hammering the server until [`Storm::halt`].
pub struct Storm {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Tally>>,
}

/// Launch `threads` workers, each looping `GET /` until halted; only 200 counts ok.
pub fn storm(addr: SocketAddr, threads: usize) -> Storm {
    let stop = Arc::new(AtomicBool::new(false));
    let handles = (0..threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut tally = Tally::new();
                while !stop.load(Ordering::Relaxed) {
                    match http_get(addr, "/", Duration::from_secs(10)) {
                        Ok((200, _)) => tally.ok += 1,
                        Ok((code, _)) => {
                            tally.failed += 1;
                            tally.last_err = Some(format!("status {code}"));
                        }
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                            tally.refused += 1;
                            tally.last_err = Some(e.to_string());
                        }
                        // tolerated connection drops: reset, zero-byte close, abort, closed write side
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::ConnectionAborted
                                    | io::ErrorKind::UnexpectedEof
                                    | io::ErrorKind::BrokenPipe
                            ) =>
                        {
                            tally.last_err = Some(e.to_string());
                        }
                        Err(e) => {
                            tally.failed += 1;
                            tally.last_err = Some(e.to_string());
                        }
                    }
                }
                tally
            })
        })
        .collect();
    Storm {
        stop,
        threads: handles,
    }
}

impl Storm {
    pub fn halt(self) -> Tally {
        self.stop.store(true, Ordering::Relaxed);
        let mut total = Tally::new();
        for h in self.threads {
            if let Ok(t) = h.join() {
                total.ok += t.ok;
                total.refused += t.refused;
                total.failed += t.failed;
                if t.last_err.is_some() {
                    total.last_err = t.last_err;
                }
            }
        }
        total
    }
}

pub fn assert_exit_code(status: Option<ExitStatus>, expected: i32, srv: &Server) {
    match status.and_then(|s| s.code()) {
        Some(code) if code == expected => {}
        Some(code) => panic!(
            "expected exit {expected} [{}], got {code} [{}]\n{}",
            code_name(expected),
            code_name(code),
            diagnostics(srv)
        ),
        None => panic!(
            "expected exit {expected} [{}], but the master was killed by a signal or is still running\n{}",
            code_name(expected),
            diagnostics(srv)
        ),
    }
}

fn code_name(code: i32) -> String {
    match code {
        MASTER_EXIT_OK => "DRAINED/OK".into(),
        MASTER_EXIT_FAILBOOT => "MASTER_FAILBOOT".into(),
        MASTER_EXIT_FORCED => "MASTER_FORCED".into(),
        other => format!("code {other}"),
    }
}

/// Worker pids + `ps` subtree + server-log tail, for failure messages.
pub fn diagnostics(srv: &Server) -> String {
    let master = srv.child.id();
    format!(
        "master pid {master}, workers {:?}\n{}\n{}",
        worker_pids(master),
        ps_snapshot(master),
        log_tail(&srv.dir)
    )
}

fn ps_snapshot(master: u32) -> String {
    let out = match Command::new("ps")
        .args(["-axo", "pid=,ppid=,state=,command"])
        .output()
    {
        Ok(o) => o,
        Err(e) => return format!("ps failed: {e}"),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = vec![format!("--- ps (master {master} subtree) ---")];
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if pid == master || ppid == master {
            lines.push(line.trim().to_owned());
        }
    }
    lines.join("\n")
}

fn log_tail(dir: &Path) -> String {
    let path = dir.join("server.log");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = content.lines().rev().take(40).collect();
    let body: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    format!(
        "--- server.log tail ({}) ---\n{body}\n--- end ---",
        path.display()
    )
}

pub fn scratch_dir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rapira-e2e-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/fixtures")
        .join(name)
}

/// `extension_dir` of the linked PHP, from the same `php-config` the build script uses (crates/php_sys/build.rs).
fn php_extension_dir() -> Option<PathBuf> {
    let bin = std::env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let out = Command::new(bin).arg("--extension-dir").output().ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// The shared object for `name`, or None when this PHP build lacks it; RAPIRA_REQUIRE_EXTS turns a demanded skip into a panic.
pub fn php_extension(name: &str) -> Option<PathBuf> {
    let p = php_extension_dir().map(|d| d.join(name));
    if let Some(p) = &p
        && p.exists()
    {
        return p.clone().into();
    }
    if let Ok(required) = std::env::var("RAPIRA_REQUIRE_EXTS") {
        let stem = name.trim_end_matches(".so");
        assert!(
            !required.split(',').any(|e| e.trim() == stem),
            "RAPIRA_REQUIRE_EXTS demands {stem}, but {name} is not at {p:?}"
        );
    }
    None
}

/// `threads` clients each issue `each` sequential `GET {path}`; returns `pick(body)` for every response.
pub fn fan_out<T: Send + 'static>(
    addr: SocketAddr,
    path: &'static str,
    threads: usize,
    each: usize,
    pick: fn(&str) -> T,
) -> Vec<T> {
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            std::thread::spawn(move || {
                let mut out = Vec::with_capacity(each);
                for _ in 0..each {
                    let (code, body) =
                        http_get(addr, path, Duration::from_secs(10)).expect("fan_out GET");
                    let body = String::from_utf8_lossy(&body);
                    assert_eq!(code, 200, "got {body:?}");
                    out.push(pick(&body));
                }
                out
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        match h.join() {
            Ok(v) => all.extend(v),
            Err(e) => std::panic::resume_unwind(e),
        }
    }
    all
}

/// The master's children must be exactly `workers`, and no worker may have a child.
pub fn assert_only_workers(srv: &Server, workers: &[u32]) {
    assert_eq!(
        worker_pids(srv.pid()),
        workers,
        "the master's children must be the workers only\n{}",
        diagnostics(srv)
    );
    for &w in workers {
        assert!(
            worker_pids(w).is_empty(),
            "worker {w} must not have child processes\n{}",
            diagnostics(srv)
        );
    }
}

/// Poll `path` until every needle appears; on deadline Err carries the file state.
pub fn wait_file_contains_all(
    path: &Path,
    needles: &[String],
    deadline: Duration,
) -> Result<(), String> {
    let end = Instant::now() + deadline;
    loop {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if needles.iter().all(|n| content.contains(n.as_str())) {
            return Ok(());
        }
        if Instant::now() >= end {
            return Err(format!(
                "missing needles in {} (exists: {})\n{content}",
                path.display(),
                path.exists()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

/// An open connection for incremental reads: the read-to-EOF helpers would block until the stream ends.
pub struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    consumed: usize,
}

impl Conn {
    pub fn open(addr: SocketAddr, timeout: Duration) -> io::Result<Self> {
        let s = TcpStream::connect_timeout(&addr, timeout)?;
        s.set_read_timeout(Some(Duration::from_millis(50)))?;
        s.set_write_timeout(Some(timeout))?;
        Ok(Self {
            s,
            buf: Vec::new(),
            consumed: 0,
        })
    }

    pub fn send(&mut self, request: &[u8]) -> io::Result<()> {
        self.s.write_all(request)?;
        self.s.flush()
    }

    /// The client walking away mid-response.
    pub fn abandon(self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }

    fn unread(&self) -> &[u8] {
        &self.buf[self.consumed..]
    }

    /// Pull bytes until `pat` shows up past the consumed mark (or `deadline`).
    fn fill_until(&mut self, pat: &[u8], deadline: Duration) -> io::Result<usize> {
        let end = std::time::Instant::now() + deadline;
        loop {
            if let Some(pos) = self.unread().windows(pat.len()).position(|w| w == pat) {
                return Ok(self.consumed + pos);
            }
            if std::time::Instant::now() >= end {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "no {pat:?} within {deadline:?}; buffered: {:?}",
                        self.unread()
                    ),
                ));
            }
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("eof before {pat:?}; buffered: {:?}", self.unread()),
                    ));
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Read one head block; interim heads are separate blocks, so call again for the next one.
    pub fn read_head(&mut self, deadline: Duration) -> io::Result<(u16, Vec<(String, String)>)> {
        let head_end = self.fill_until(b"\r\n\r\n", deadline)?;
        let head = &self.buf[self.consumed..head_end];
        let text = String::from_utf8_lossy(head).into_owned();
        self.consumed = head_end + 4;
        let mut lines = text.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad status line in {text:?}"),
                )
            })?;
        let fields = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        Ok((status, fields))
    }

    /// Block until `pat` is on the wire while the response is still open; consumes through it.
    pub fn read_body_until(&mut self, pat: &[u8], deadline: Duration) -> io::Result<()> {
        let pos = self.fill_until(pat, deadline)?;
        self.consumed = pos + pat.len();
        Ok(())
    }

    /// Read exactly `n` bytes: content-length framing on a connection that stays open.
    pub fn read_n(&mut self, n: usize, deadline: Duration) -> io::Result<Vec<u8>> {
        let end = std::time::Instant::now() + deadline;
        while self.unread().len() < n {
            if std::time::Instant::now() >= end {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{n} bytes not on the wire; buffered: {:?}", self.unread()),
                ));
            }
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof mid body"));
                }
                Ok(m) => self.buf.extend_from_slice(&chunk[..m]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
        let out = self.unread()[..n].to_vec();
        self.consumed += n;
        Ok(out)
    }

    /// Drain to EOF and return everything unread.
    pub fn read_remaining(&mut self, deadline: Duration) -> io::Result<Vec<u8>> {
        let end = std::time::Instant::now() + deadline;
        loop {
            if std::time::Instant::now() >= end {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no eof in time"));
            }
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
        let rest = self.unread().to_vec();
        self.consumed = self.buf.len();
        Ok(rest)
    }
}

/// Decode a chunked body into `(chunks, trailer-section bytes)`; a missing terminating zero chunk is the truncation signal.
pub fn decode_chunked(mut raw: &[u8]) -> io::Result<(Vec<Vec<u8>>, Vec<u8>)> {
    let mut chunks = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no chunk-size line"))?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&raw[..line_end])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?
                .trim(),
            16,
        )
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            let trailer = raw.strip_suffix(b"\r\n").unwrap_or(raw).to_vec();
            return Ok((chunks, trailer));
        }
        if raw.len() < size + 2 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short chunk"));
        }
        chunks.push(raw[..size].to_vec());
        raw = &raw[size + 2..];
    }
}

/// Poll `server.log` for `needle`, bounded.
pub fn wait_log_contains(srv: &Server, needle: &str, deadline: Duration) -> bool {
    let path = srv.dir.join("server.log");
    let end = std::time::Instant::now() + deadline;
    loop {
        if std::fs::read_to_string(&path)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
        {
            return true;
        }
        if std::time::Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
