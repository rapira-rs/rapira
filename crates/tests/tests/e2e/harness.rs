use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rapira_config::Mode;
use rapira_net::ListenAddr;
use serde_json::Value;
use tests::{StatusBody, poll, server_log};

/// The boot timeout includes slow macOS CI hosts.
pub const BOOT: Duration = Duration::from_secs(30);

/// A worker reached the boot failure limit before its pool served.
pub const EXIT_FAILBOOT: i32 = 70;
pub const EXIT_OK: i32 = 0;

/// This includes the default `server.stop_timeout_secs` of 30 seconds.
pub const STOP_BUDGET: Duration = Duration::from_secs(45);

/// A server and the directory that holds its config and log.
pub struct Server {
    child: Child,
    /// The TCP address of the first pool. A Unix listener has no TCP address here.
    pub addr: SocketAddr,
    pub dir: PathBuf,
    /// The first PHP pool in the config.
    first_pool: &'static str,
    /// The listener of the `[grpc]` pool, when the config has one.
    pub grpc: Option<ListenAddr>,
}

impl Server {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The stdout and stderr of the server.
    pub fn log_file(&self) -> PathBuf {
        self.dir.join("server.log")
    }

    /// Sends SIGQUIT and waits for the server to exit. The log stays available until drop.
    pub fn stop(&mut self) {
        signal(self.pid(), libc::SIGQUIT);
        let status = self.wait_exit(STOP_BUDGET);
        assert_exit_code(status, EXIT_OK, self);
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        // A try_wait error ends the poll, because transpose gives Some(Err). Result::ok then maps the error to None.
        poll(timeout, || self.child.try_wait().transpose()).and_then(Result::ok)
    }

    pub fn try_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for Server {
    /// Signal only an unreaped process. The kernel can reuse a reaped PID.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            signal(self.child.id(), libc::SIGTERM);
            if self.wait_exit(Duration::from_secs(5)).is_none() {
                let _ = self.child.kill();
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

/// The root package builds the binary. Use `RAPIRA_BIN`, or find `rapira` in a directory above the test binary. Make a relative path absolute because the server starts in the scratch directory.
fn rapira_bin() -> PathBuf {
    if let Ok(p) = std::env::var("RAPIRA_BIN") {
        if !p.contains('/') {
            return PathBuf::from(p);
        }
        return std::path::absolute(p).expect("absolute RAPIRA_BIN");
    }
    let exe = std::env::current_exe().expect("current_exe");
    // the depth of the test binary below target/<profile> depends on the cargo version
    for dir in exe.ancestors() {
        let bin = dir.join("rapira");
        if bin.is_file() {
            return bin;
        }
    }
    panic!(
        "rapira binary not found above {}; build it first (cargo build -p rapira_core --bin rapira) or set RAPIRA_BIN",
        exe.display()
    );
}

/// The version that `rapira --version` prints.
pub fn rapira_version() -> String {
    let out = Command::new(rapira_bin())
        .arg("--version")
        .output()
        .expect("run rapira --version");
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .strip_prefix("rapira ")
        .unwrap_or_else(|| panic!("unexpected --version output {text:?}"))
        .to_owned()
}

/// PHP_VERSION of the PHP that `php-config` names. The build links the libphp of that PHP.
pub fn php_version() -> String {
    php_config("--version").expect("run php-config --version")
}

/// The thread safety setting of the PHP build that `php-config` names.
pub fn is_zts() -> bool {
    cfg!(php_zts)
}

/// The `listen` value of a TCP pool on `port`.
fn tcp(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Tries up to three ports. Another process can bind a port between its allocation and the server boot.
fn spawn_ready(spawn: &Spawn, dir: PathBuf, render: &dyn Fn(u16) -> String) -> Server {
    let mut last_log = String::new();
    for _ in 0..3 {
        let Attempt { mut child, addr } = spawn_attempt(spawn, &dir, render);
        let first_pool = spawn.first_pool();
        let listen = match first_pool {
            "http" => &spawn.http_listen,
            _ => &spawn.grpc_listen,
        };
        let listen = listen.clone().unwrap_or(ListenAddr::Tcp(addr));
        if let Some(bound) = wait_for_listener(listen, first_pool, &dir, &mut child) {
            return Server {
                child,
                addr: match bound {
                    ListenAddr::Tcp(bound) => bound,
                    ListenAddr::Unix(_) => addr,
                },
                dir,
                first_pool,
                grpc: None,
            };
        }
        let _ = child.kill();
        let _ = child.wait();
        last_log = log_tail(&dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    panic!("rapira never accepted a connection after 3 attempts\n{last_log}");
}

/// One attempt to start the server.
struct Attempt {
    child: Child,
    /// The TCP address of the port that `render` got.
    addr: SocketAddr,
}

/// Starts the server in `dir` with a config for a fresh port. The caller decides how to wait.
fn spawn_attempt(spawn: &Spawn, dir: &Path, render: &dyn Fn(u16) -> String) -> Attempt {
    let port = free_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    std::fs::write(dir.join("rapira.toml"), render(port)).expect("write config");
    let log = File::create(dir.join("server.log")).expect("create server.log");
    let mut cmd = Command::new(rapira_bin());
    cmd.arg("serve").arg(dir.join("rapira.toml"));
    cmd.current_dir(dir);
    if spawn.phprc {
        cmd.env("PHPRC", dir);
    } else {
        cmd.env_remove("PHPRC");
    }
    match &spawn.rust_log {
        Some(v) => cmd.env("RUST_LOG", v),
        None => cmd.env_remove("RUST_LOG"),
    };
    for (k, v) in &spawn.env {
        cmd.env(k, v);
    }
    let child = cmd
        .stdout(Stdio::from(log.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn rapira");
    Attempt { child, addr }
}

/// A server that exited.
pub struct Exited {
    pub status: ExitStatus,
    /// The whole server.log.
    pub log: String,
}

/// One spawn that must exit. [`Exited::log`] holds the whole log and not only the tail. With `RUST_BACKTRACE` set, the backtrace after the error line is longer than the tail window.
fn exit_of(spawn: &Spawn, dir: PathBuf, render: &dyn Fn(u16) -> String) -> Exited {
    let Attempt { child, addr } = spawn_attempt(spawn, &dir, render);
    let mut srv = Server {
        child,
        addr,
        dir,
        first_pool: spawn.first_pool(),
        grpc: None,
    };
    let Some(status) = srv.wait_exit(BOOT) else {
        panic!("rapira did not exit");
    };
    let log = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
    Exited { status, log }
}

/// Renders `[http.pool]` before `[http]`.
/// TOML accepts an explicit super-table header after its sub-table.
/// `http_extra` renders last, so a header it opens ends the file.
fn render_config(
    listen: &str,
    workers: usize,
    fixture: &str,
    http_extra: &str,
    extra: &str,
) -> String {
    format!(
        "[http.pool]\nworkers = {workers}\nentrypoint = \"{fixture}\"\n{extra}\n[http]\nlisten = \"{listen}\"\n{http_extra}"
    )
}

pub use tests::grpc::ECHO_SERVICE;

/// Renders a `[grpc]` pool with `workers` threads. A `None` value for `services` leaves the key out.
fn render_grpc(
    listen: &str,
    workers: usize,
    entrypoint: &str,
    descriptor_set: &str,
    services: Option<&[String]>,
    extra: &str,
    pool: &str,
) -> String {
    let services = services.map_or(String::new(), |names| {
        let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
        format!("services = [{}]\n", quoted.join(", "))
    });
    format!(
        "[grpc]\nlisten = \"{listen}\"\ndescriptor_set = \"{descriptor_set}\"\n{services}{extra}\n\
         [grpc.pool]\nworkers = {workers}\nentrypoint = \"{entrypoint}\"\n{pool}"
    )
}

/// [`Spawn::http`] or [`Spawn::grpc`] starts the config. Each pool has one worker unless [`Spawn::workers`] sets its count.
/// Each fixture directory is copied to `http/` or `grpc/`, so the fixture can include its siblings. The server starts in the scratch directory.
/// PHPRC names a `php.ini` in the scratch dir with the contents of `crates/tests/fixtures/ini/shared/php.ini`, unless [`Spawn::php_ini`] replaces them or [`Spawn::no_phprc`] leaves PHPRC unset.
pub struct Spawn {
    http: Option<HttpPool>,
    /// The listener of the `[http]` pool; None listens on a TCP port that the harness picks.
    http_listen: Option<ListenAddr>,
    grpc: Option<PathBuf>,
    /// The listener of the `[grpc]` pool; None listens on a TCP port that the harness picks.
    grpc_listen: Option<ListenAddr>,
    /// The `[grpc] services` list; None leaves the key out.
    services: Option<Vec<String>>,
    /// The `[grpc] descriptor_set`; None stages `echo.binpb`.
    descriptor_set: Option<PathBuf>,
    /// Number of worker threads in each pool.
    workers: usize,
    http_extra: String,
    /// Keys inside `[http.pool]`, after `mode`.
    http_pool: String,
    grpc_extra: String,
    /// Keys inside `[grpc.pool]`.
    grpc_pool: String,
    toml: String,
    php_ini: String,
    /// False leaves PHPRC unset, so PHP reads its compiled-in php.ini.
    phprc: bool,
    env: Vec<(String, String)>,
    /// The RUST_LOG filter; None removes RUST_LOG from the environment, so the `[log]` section sets the filter.
    rust_log: Option<String>,
}

/// The `[http]` pool of a [`Spawn`].
struct HttpPool {
    mode: Mode,
    entrypoint: Entrypoint,
}

/// The `entrypoint` of the `[http]` pool.
enum Entrypoint {
    /// A fixture whose directory the spawn copies into the scratch dir.
    Fixture(PathBuf),
    /// A path that goes into the config as given, with nothing staged.
    Verbatim(String),
}

impl Spawn {
    /// An `[http]` pool in `mode` over `fixture`. `Server::addr` is its listener.
    pub fn http(mode: Mode, fixture: impl Into<PathBuf>) -> Spawn {
        Spawn {
            http: Some(HttpPool {
                mode,
                entrypoint: Entrypoint::Fixture(fixture.into()),
            }),
            ..Spawn::empty()
        }
    }

    /// An `[http]` pool in `mode` whose `entrypoint` is `path` as given, with nothing staged, so a path that no fixture can stage reaches the boot check.
    pub fn http_entrypoint(mode: Mode, path: &str) -> Spawn {
        Spawn {
            http: Some(HttpPool {
                mode,
                entrypoint: Entrypoint::Verbatim(path.to_owned()),
            }),
            ..Spawn::empty()
        }
    }

    /// A `[grpc]` pool over `fixture` that serves `rapira.test.v1.EchoService` out of `echo.binpb`. `Server::grpc` is its listener, and `Server::addr` too when it listens on TCP.
    pub fn grpc(fixture: impl Into<PathBuf>) -> Spawn {
        Spawn {
            grpc: Some(fixture.into()),
            ..Spawn::empty()
        }
    }

    fn empty() -> Spawn {
        let ini = tests::fixture("ini/shared/php.ini");
        Spawn {
            http: None,
            http_listen: None,
            grpc: None,
            grpc_listen: None,
            services: Some(vec![ECHO_SERVICE.to_owned()]),
            descriptor_set: None,
            workers: 1,
            http_extra: String::new(),
            http_pool: String::new(),
            grpc_extra: String::new(),
            grpc_pool: String::new(),
            toml: String::new(),
            php_ini: std::fs::read_to_string(&ini)
                .unwrap_or_else(|e| panic!("read {}: {e}", ini.display())),
            phprc: true,
            env: Vec::new(),
            rust_log: Some("info".to_owned()),
        }
    }

    /// Adds the `[grpc]` pool of [`Spawn::grpc`] next to the `[http]` pool. `Server::grpc` is its listener.
    pub fn with_grpc(mut self, fixture: impl Into<PathBuf>) -> Spawn {
        self.grpc = Some(fixture.into());
        self
    }

    /// The `[http]` pool listens on `listen`.
    pub fn http_listen(mut self, listen: ListenAddr) -> Spawn {
        self.http_listen = Some(listen);
        self
    }

    /// The `[grpc]` pool listens on `listen`.
    pub fn grpc_listen(mut self, listen: ListenAddr) -> Spawn {
        self.grpc_listen = Some(listen);
        self
    }

    /// The `[grpc] services` list in place of `EchoService`. None leaves the key out, so the pool serves the services of the files that no other file imports.
    pub fn services(mut self, services: Option<&[&str]>) -> Spawn {
        self.services = services.map(|names| names.iter().map(|&n| n.to_owned()).collect());
        self
    }

    /// The `[grpc] descriptor_set` in place of `echo.binpb`.
    pub fn descriptor_set(mut self, path: &Path) -> Spawn {
        self.descriptor_set = Some(path.to_owned());
        self
    }

    /// Sets the number of worker threads in each pool.
    pub fn workers(mut self, count: usize) -> Spawn {
        self.workers = count;
        self
    }

    /// Keys inside `[http]`, for example `server_port = 8080`. The text may open `[http.*]` tables after its keys.
    pub fn http_extra(mut self, keys: &str) -> Spawn {
        self.http_extra += keys;
        self.http_extra.push('\n');
        self
    }

    /// Keys inside `[http.pool]`, for example `max_requests = 2`.
    pub fn http_pool(mut self, keys: &str) -> Spawn {
        self.http_pool += keys;
        self.http_pool.push('\n');
        self
    }

    /// Keys inside `[grpc]`, for example `reflection = true`. The text may open `[grpc.*]` tables after its keys.
    pub fn grpc_extra(mut self, keys: &str) -> Spawn {
        self.grpc_extra += keys;
        self.grpc_extra.push('\n');
        self
    }

    /// Keys inside `[grpc.pool]`, for example `max_boot_failures = 0`.
    pub fn grpc_pool(mut self, keys: &str) -> Spawn {
        self.grpc_pool += keys;
        self.grpc_pool.push('\n');
        self
    }

    /// Top-level tables at the end of the config, for example `[server]`.
    pub fn toml(mut self, tables: &str) -> Spawn {
        self.toml += tables;
        self.toml.push('\n');
        self
    }

    /// `[log] format = "json"`, for the readers in `tests::server_log`. RUST_LOG still selects the records.
    pub fn json_log(self) -> Spawn {
        self.toml("[log]\nformat = \"json\"")
    }

    /// The contents of the `php.ini` in the scratch dir.
    pub fn php_ini(mut self, contents: &str) -> Spawn {
        contents.clone_into(&mut self.php_ini);
        self
    }

    /// Leaves PHPRC unset. PHP then reads the php.ini at its configured path.
    pub fn no_phprc(mut self) -> Spawn {
        self.phprc = false;
        self
    }

    /// An environment variable of the server.
    pub fn env(mut self, key: &str, value: &str) -> Spawn {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The RUST_LOG filter of the server. The default is `info`.
    pub fn rust_log(mut self, filter: &str) -> Spawn {
        self.rust_log = Some(filter.to_owned());
        self
    }

    /// Removes RUST_LOG from the environment, so the `[log]` section sets the filter.
    pub fn no_rust_log(mut self) -> Spawn {
        self.rust_log = None;
        self
    }

    /// Starts the server and waits for its first listener. Use [`wait_serving`] to wait for PHP.
    pub fn spawn(self) -> Server {
        let grpc_port = Cell::new(0);
        let dir = scratch_dir();
        let render = self.stage(&dir, &grpc_port);
        let mut srv = spawn_ready(&self, dir, &render);
        if self.grpc.is_some() {
            srv.grpc = Some(match &self.grpc_listen {
                Some(ListenAddr::Tcp(addr)) if self.http.is_none() && addr.port() == 0 => {
                    ListenAddr::Tcp(srv.addr)
                }
                Some(listen) => listen.clone(),
                None => ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], grpc_port.get()))),
            });
        }
        srv
    }

    fn first_pool(&self) -> &'static str {
        if self.http.is_some() { "http" } else { "grpc" }
    }

    /// Starts a server that must fail its boot and waits for its exit.
    pub fn boot_failure(self) -> Exited {
        let grpc_port = Cell::new(0);
        let dir = scratch_dir();
        let render = self.stage(&dir, &grpc_port);
        exit_of(&self, dir, &render)
    }

    /// Fills the scratch dir `dir` and returns the config for a port; `grpc_port` gets the port of the `[grpc]` pool.
    fn stage<'a>(
        &'a self,
        dir: &Path,
        grpc_port: &'a Cell<u16>,
    ) -> impl Fn(u16) -> String + use<'a> {
        struct StagedHttp {
            mode: Mode,
            entrypoint: String,
        }
        struct StagedGrpc {
            entrypoint: String,
            descriptor_set: String,
        }
        std::fs::write(dir.join("php.ini"), &self.php_ini).expect("write php.ini");
        let http = self.http.as_ref().map(|pool| StagedHttp {
            mode: pool.mode,
            entrypoint: match &pool.entrypoint {
                Entrypoint::Fixture(fixture) => stage_dir(dir, "http", fixture),
                Entrypoint::Verbatim(path) => path.clone(),
            },
        });
        let grpc = self.grpc.as_ref().map(|fixture| {
            let descriptor_set = match &self.descriptor_set {
                Some(path) => path.display().to_string(),
                None => {
                    copy_file(&tests::echo_descriptor_set(), &dir.join("echo.binpb"))
                        .expect("copy echo.binpb");
                    "echo.binpb".to_owned()
                }
            };
            StagedGrpc {
                entrypoint: stage_dir(dir, "grpc", fixture),
                descriptor_set,
            }
        });
        move |port| {
            // The first pool without a listen address gets this port.
            let mut port = Some(port);
            let mut config = String::new();
            if let Some(StagedHttp { mode, entrypoint }) = &http {
                let listen = match &self.http_listen {
                    Some(listen) => listen.to_string(),
                    None => tcp(port.take().unwrap_or_else(free_port)),
                };
                let pool = format!("mode = \"{mode}\"\n{}", self.http_pool);
                config +=
                    &render_config(&listen, self.workers, entrypoint, &self.http_extra, &pool);
                config.push('\n');
            }
            if let Some(StagedGrpc {
                entrypoint,
                descriptor_set,
            }) = &grpc
            {
                let listen = match &self.grpc_listen {
                    Some(listen) => listen.to_string(),
                    None => {
                        grpc_port.set(port.take().unwrap_or_else(free_port));
                        tcp(grpc_port.get())
                    }
                };
                config += &render_grpc(
                    &listen,
                    self.workers,
                    entrypoint,
                    descriptor_set,
                    self.services.as_deref(),
                    &self.grpc_extra,
                    &self.grpc_pool,
                );
                config.push('\n');
            }
            config + &self.toml
        }
    }
}

/// Copies a file with a read and a write. `std::fs::copy` calls `copy_file_range`, and a libphp with zend_test replaces that symbol with a wrapper that needs a started PHP thread.
pub fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::write(to, std::fs::read(from)?)
}

/// Copies the files of the directory of `fixture` into `dir/sub`; returns the entrypoint relative to `dir`.
fn stage_dir(dir: &Path, sub: &str, fixture: &Path) -> String {
    let name = fixture
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| panic!("fixture {} has no utf-8 file name", fixture.display()));
    let from = fixture.parent().expect("fixture has a directory");
    let to = dir.join(sub);
    std::fs::create_dir_all(&to).expect("create the fixture dir");
    for entry in std::fs::read_dir(from).unwrap_or_else(|e| panic!("read {}: {e}", from.display()))
    {
        let path = entry.expect("fixture dir entry").path();
        if path.is_file() {
            copy_file(&path, &to.join(path.file_name().expect("file name")))
                .unwrap_or_else(|e| panic!("copy {}: {e}", path.display()));
        }
    }
    format!("{sub}/{name}")
}

/// Port zero takes its bound address from the listener log.
fn logged_tcp_addr(dir: &Path, pool: &str) -> Option<SocketAddr> {
    let log = std::fs::read_to_string(dir.join("server.log")).ok()?;
    let prefix = format!("{pool}: listening on ");
    log.lines().find_map(|line| {
        if let Some((_, addr)) = line.split_once(&prefix) {
            return addr.parse().ok();
        }
        let record: Value = serde_json::from_str(line).ok()?;
        if record["target"] != pool {
            return None;
        }
        record["fields"]["message"]
            .as_str()?
            .strip_prefix("listening on ")?
            .parse()
            .ok()
    })
}

fn wait_for_listener(
    mut listen: ListenAddr,
    pool: &str,
    dir: &Path,
    child: &mut Child,
) -> Option<ListenAddr> {
    poll(BOOT, || {
        if child.try_wait().expect("check server exit").is_some() {
            return Some(None);
        }
        if matches!(&listen, ListenAddr::Tcp(addr) if addr.port() == 0) {
            listen = ListenAddr::Tcp(logged_tcp_addr(dir, pool)?);
        }
        let connected = match &listen {
            ListenAddr::Tcp(addr) => {
                TcpStream::connect_timeout(addr, Duration::from_millis(200)).is_ok()
            }
            ListenAddr::Unix(path) => UnixStream::connect(path).is_ok(),
        };
        connected.then(|| Some(listen.clone()))
    })
    .flatten()
}

/// An HTTP/1.1 GET with `Connection: close`. The body is close-delimited, so the client needs no chunked or keep-alive parsing.
pub fn http_get(
    addr: SocketAddr,
    path: &str,
    timeout: Duration,
) -> io::Result<StatusBody<Vec<u8>>> {
    http_get_with_headers(addr, path, &[], timeout)
}

/// [`http_get`] plus extra request fields, written in the order given so a repeated name stays repeated on the wire.
pub fn http_get_with_headers(
    addr: SocketAddr,
    path: &str,
    fields: &[(&str, &str)],
    timeout: Duration,
) -> io::Result<StatusBody<Vec<u8>>> {
    let raw = http_get_raw(addr, path, fields, timeout)?;
    parse_status_and_body(&raw).map(|StatusBody { status, body }| StatusBody {
        status,
        body: body.to_vec(),
    })
}

/// The whole response with its head, for assertions about which fields reached the client.
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

/// Like [`http_raw`], but returns the unparsed response bytes, for assertions on the header block itself.
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

/// Sends a request that the caller writes in full. The client adds no Host or Connection line.
pub fn http_raw(
    addr: SocketAddr,
    request: &[u8],
    timeout: Duration,
) -> io::Result<StatusBody<Vec<u8>>> {
    let raw = http_raw_bytes(addr, request, timeout)?;
    parse_status_and_body(&raw).map(|StatusBody { status, body }| StatusBody {
        status,
        body: body.to_vec(),
    })
}

/// A POST like [`http_get`], with a body. `content_type` is bytes, because a multipart boundary is opaque octets and obs-text is legal in a field value.
pub fn http_post(
    addr: SocketAddr,
    path: &str,
    content_type: &[u8],
    body: &[u8],
    timeout: Duration,
) -> io::Result<StatusBody<Vec<u8>>> {
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

/// The status code and the body of a close-delimited response.
pub fn parse_status_and_body(raw: &[u8]) -> io::Result<StatusBody<&[u8]>> {
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
    Ok(StatusBody {
        status: code,
        body: &raw[head_end + 4..],
    })
}

/// A request body of `len` bytes where byte i is i % 251. A dropped or reordered transport chunk changes the bytes.
pub fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

pub fn signal(pid: u32, sig: i32) {
    // SAFETY: kill is a plain syscall; a stale pid returns ESRCH, which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Per-thread outcome counters. `refused` means the listener closed, and `failed` means a non-200 response, a hang, or a corrupt reply. A connection drop counts in `dropped` and not in `failed`, because the balancer retries a dropped connection. It also records `last_err`.
#[derive(Default)]
pub struct Tally {
    pub ok: u64,
    pub refused: u64,
    pub failed: u64,
    /// Connections that closed without a response.
    pub dropped: u64,
    pub last_err: Option<String>,
}

/// Threads that send requests to the server until [`Storm::halt`].
pub struct Storm {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Tally>>,
}

/// Starts `threads` threads. Each thread sends `GET /` in a loop until the halt. Only a 200 counts as ok.
pub fn storm(addr: SocketAddr, threads: usize) -> Storm {
    let stop = Arc::new(AtomicBool::new(false));
    let handles = (0..threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut tally = Tally::default();
                while !stop.load(Ordering::Relaxed) {
                    match http_get(addr, "/", Duration::from_secs(10)) {
                        Ok(StatusBody { status: 200, .. }) => tally.ok += 1,
                        Ok(StatusBody { status: code, .. }) => {
                            tally.failed += 1;
                            tally.last_err = Some(format!("status {code}"));
                        }
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                            tally.refused += 1;
                            tally.last_err = Some(e.to_string());
                        }
                        // connection drops: reset, zero-byte close, abort, closed write side
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::ConnectionAborted
                                    | io::ErrorKind::UnexpectedEof
                                    | io::ErrorKind::BrokenPipe
                            ) =>
                        {
                            tally.dropped += 1;
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
        let mut total = Tally::default();
        for h in self.threads {
            if let Ok(t) = h.join() {
                total.ok += t.ok;
                total.refused += t.refused;
                total.failed += t.failed;
                total.dropped += t.dropped;
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
        Some(code) => panic!("expected exit {expected}, got {code}\n{}", diagnostics(srv)),
        None => panic!(
            "expected exit {expected}, but the server got a fatal signal or still runs\n{}",
            diagnostics(srv)
        ),
    }
}

/// The server PID and recent log records for failure messages.
pub fn diagnostics(srv: &Server) -> String {
    format!("server pid {}\n{}", srv.pid(), log_tail(&srv.dir))
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

/// Trimmed stdout of the `php-config` that the build script uses (crates/sapi/build.rs), or None when it fails.
fn php_config(arg: &str) -> Option<String> {
    let bin = std::env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let out = Command::new(bin).arg(arg).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `extension_dir` of the linked PHP.
fn php_extension_dir() -> Option<PathBuf> {
    php_config("--extension-dir").map(PathBuf::from)
}

/// The shared object for `name`, or None when this PHP build lacks it.
pub fn php_extension(name: &str) -> Option<PathBuf> {
    let p = php_extension_dir().map(|d| d.join(name));
    if let Some(p) = &p
        && p.exists()
    {
        return p.clone().into();
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
                    let StatusBody { status: code, body } =
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

/// Polls `path` until every needle appears. At the deadline, the Err holds the file state.
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

/// A port that no listener holds when this returns.
pub fn free_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

/// One response head that [`Conn::read_head`] read.
pub struct Head {
    pub status: u16,
    /// The fields in wire order: lowercase names, trimmed values.
    pub fields: Vec<(String, String)>,
}

/// An open connection for incremental reads. The read-to-EOF helpers block until the stream ends.
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

    /// Closes both directions of the connection, as a client that leaves in the middle of a response.
    pub fn leave(self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }

    fn unread(&self) -> &[u8] {
        &self.buf[self.consumed..]
    }

    /// Reads bytes until `pat` appears after the consumed mark, or until `deadline` passes.
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

    /// Reads one head block. Interim heads are separate blocks, so call again for the next one.
    pub fn read_head(&mut self, deadline: Duration) -> io::Result<Head> {
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
        Ok(Head { status, fields })
    }

    /// Blocks until `pat` is on the wire while the response is still open. Consumes the bytes up to the end of `pat`.
    pub fn read_body_until(&mut self, pat: &[u8], deadline: Duration) -> io::Result<()> {
        let pos = self.fill_until(pat, deadline)?;
        self.consumed = pos + pat.len();
        Ok(())
    }

    /// Reads exactly `n` bytes, for content-length framing on a connection that stays open.
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

    /// Reads to EOF and returns everything unread.
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

/// Polls `server.log` for `needle` until `deadline`.
pub fn wait_log_contains(srv: &Server, needle: &str, deadline: Duration) -> bool {
    let path = srv.dir.join("server.log");
    poll(deadline, || {
        std::fs::read_to_string(&path)
            .is_ok_and(|s| s.contains(needle))
            .then_some(())
    })
    .is_some()
}

/// Reads one complete pool snapshot per signal. A new search starts at the current end of the log.
fn with_status<T>(
    srv: &Server,
    pool: &str,
    what: &str,
    check: impl Fn(&str, &[&str]) -> Option<T>,
) -> T {
    let mut log = File::open(srv.log_file()).expect("open server.log");
    let header = format!("pool={pool} workers=");
    let worker = format!("pool={pool} worker=");
    let deadline = Instant::now() + BOOT;
    while Instant::now() < deadline {
        log.seek(SeekFrom::End(0))
            .expect("seek to the next snapshot");
        signal(srv.pid(), libc::SIGUSR1);
        let mut text = String::new();
        let snapshot = poll(deadline.saturating_duration_since(Instant::now()), || {
            log.read_to_string(&mut text).expect("read server.log");
            let (complete, _) = text.rsplit_once('\n')?;
            let mut lines = complete[complete.rfind(&header)?..].lines();
            let summary = lines.next()?;
            let count: usize = summary
                .strip_prefix(&header)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            let rows: Vec<_> = lines.filter(|line| line.contains(&worker)).collect();
            (rows.len() == count).then(|| check(summary, &rows))
        });
        match snapshot {
            Some(Some(result)) => return result,
            Some(None) => std::thread::sleep(Duration::from_millis(20)),
            None => break,
        }
    }
    panic!(
        "no {what} for pool {pool} within {BOOT:?}\n{}",
        diagnostics(srv)
    );
}

/// True when the status line shows an idle or active worker.
fn is_serving(line: &str) -> bool {
    line.contains(" state=idle ") || line.contains(" state=active ")
}

/// Waits until at least `count` workers in the pool are idle or active in one snapshot.
pub fn wait_serving(srv: &Server, pool: &str, count: usize) {
    with_status(srv, pool, &format!("{count} serving workers"), |_, rows| {
        let serving = rows.iter().filter(|line| is_serving(line)).count();
        (serving >= count).then_some(())
    });
}

/// Waits until every worker in the pool serves and has recycled its app, in one snapshot.
pub fn wait_recycled(srv: &Server, pool: &str) {
    with_status(srv, pool, "a recycle of every worker", |_, rows| {
        rows.iter()
            .all(|line| is_serving(line) && !line.contains(" recycles=0 "))
            .then_some(())
    });
}

/// Returns a fresh status line for worker zero of the first pool.
pub fn slot_line(srv: &Server, fragment: &str) -> String {
    with_status(srv, srv.first_pool, fragment, |_, rows| {
        rows.iter()
            .find(|line| line.contains(" worker=0 ") && line.contains(fragment))
            .map(|line| (*line).to_owned())
    })
}

/// Returns a fresh pool summary that contains `fragment`.
pub fn pool_line(srv: &Server, pool: &str, fragment: &str) -> String {
    with_status(srv, pool, fragment, |summary, _| {
        summary.contains(fragment).then(|| summary.to_owned())
    })
}

/// The listener of the `[grpc]` pool of `srv`.
pub fn listen(srv: &Server) -> ListenAddr {
    srv.grpc.clone().expect("the config has a [grpc] pool")
}

/// The context of each `call` record that `grpc/wire-worker.php` logged, in log order. The fixture logs before it answers, so a call that has its response is in the log.
pub fn calls(srv: &Server) -> Vec<Value> {
    server_log::app_contexts(&srv.log_file(), "call")
        .iter()
        .map(|c| serde_json::from_str(c).expect("a call record holds JSON"))
        .collect()
}

/// The server error record when the gRPC plugin returns an error.
pub fn plugin_error(srv: &Server) -> Option<String> {
    server_log::records(&srv.log_file())
        .into_iter()
        .find(|c| c.target == "rapira" && c.message.starts_with("plugin grpc"))
        .map(|c| c.message)
}

/// The `remote` that PHP reports for a client at `peer`. `Conn::open` connects from an unnamed unix socket, which PHP reports as `unix:NULL`.
pub fn logged_remote(peer: Option<SocketAddr>) -> String {
    peer.map_or_else(|| "unix:NULL".to_owned(), |addr| addr.to_string())
}

/// Waits for exit 0 on a blocking thread. The clients on the test runtime can continue during the drain.
pub async fn exited(mut srv: Server) -> Server {
    tokio::task::spawn_blocking(move || {
        let status = srv.wait_exit(STOP_BUDGET);
        assert_exit_code(status, EXIT_OK, &srv);
        srv
    })
    .await
    .expect("the wait task")
}

/// Sends SIGQUIT, then waits in [`exited`].
pub async fn stop(srv: Server) -> Server {
    signal(srv.pid(), libc::SIGQUIT);
    exited(srv).await
}
