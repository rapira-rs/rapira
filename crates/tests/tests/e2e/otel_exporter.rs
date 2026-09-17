use crate::harness::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

struct RejectingCollector {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RejectingCollector {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let mut length = 0;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_ok() {
                    let _ = reader.get_mut().write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for RejectingCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
        let _ = self.thread.take().unwrap().join();
    }
}

#[test]
fn exporter_failures_use_json_and_the_parent_filter() {
    struct Case {
        name: &'static str,
        level: &'static str,
        reqwest_level: &'static str,
        rust_log: bool,
        connection_debug: bool,
    }
    let cases = [
        Case {
            name: "JSON exporter failure at error level",
            level: "error",
            reqwest_level: "error",
            rust_log: false,
            connection_debug: false,
        },
        Case {
            name: "configured target enables exporter HTTP diagnostics",
            level: "error",
            reqwest_level: "debug",
            rust_log: false,
            connection_debug: true,
        },
        Case {
            name: "configured target suppresses exporter HTTP diagnostics",
            level: "debug",
            reqwest_level: "error",
            rust_log: false,
            connection_debug: false,
        },
        Case {
            name: "RUST_LOG replaces configured target levels",
            level: "debug",
            reqwest_level: "debug",
            rust_log: true,
            connection_debug: false,
        },
    ];
    for case in cases {
        let collector = RejectingCollector::start();
        let config = format!(
            "mode = \"dispatcher\"\n[otel]\nenabled = true\nendpoint = \"http://{}\"\nlogs = false\nmetrics = false\nflush_interval_ms = 20\n[log]\nformat = \"json\"\nlevel = \"{}\"\n[log.targets]\nreqwest = \"{}\"\n",
            collector.addr, case.level, case.reqwest_level,
        );
        let mut srv = if case.rust_log {
            spawn_with_config("shared/echo-worker.php", 1, &config)
        } else {
            spawn_without_rust_log("shared/echo-worker.php", 1, &config)
        };
        let (status, _) = http_get(srv.addr, "/", TIMEOUT).unwrap();
        assert_eq!(status, 200, "{}: {}", case.name, diagnostics(&srv));
        assert!(
            wait_log_contains(&srv, "export failed", TIMEOUT),
            "{}: {}",
            case.name,
            diagnostics(&srv)
        );
        signal(srv.pid(), libc::SIGQUIT);
        assert_exit_code(srv.wait_exit(TIMEOUT), MASTER_EXIT_OK, &srv);
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap();
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("{}: invalid JSON: {error}: {line}", case.name))
            })
            .collect();
        assert!(
            records.iter().any(|record| record["target"] == "otel"
                && record["fields"]["message"] == "export failed"),
            "{}: {text}",
            case.name
        );
        let connection_debug = records.iter().any(|record| {
            record["level"] == "DEBUG"
                && record["target"]
                    .as_str()
                    .is_some_and(|target| target.starts_with("reqwest::"))
        });
        assert_eq!(
            connection_debug, case.connection_debug,
            "{}: {text}",
            case.name
        );
    }
}
