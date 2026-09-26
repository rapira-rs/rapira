use crate::harness::*;
use rapira_sapi::Mode;
use std::time::{Duration, Instant};

/// `[log] format = "json"` shapes every record while `RUST_LOG` still owns the filter.
#[test]
fn json_format_shapes_the_log() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "[log]\nformat = \"json\"\n");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        let banner = text.lines().any(|l| {
            serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| {
                v["target"] == "rapira"
                    && v["fields"]["message"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("rapira_core v"))
            })
        });

        let complete = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
        for l in complete.lines().filter(|l| !l.is_empty()) {
            assert!(
                serde_json::from_str::<serde_json::Value>(l).is_ok(),
                "non-JSON line in json mode: {l}\n{}",
                diagnostics(&srv)
            );
        }
        if banner {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no JSON banner line in server.log\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Default `[log]` on a redirected (non-tty) stderr emits no ANSI escapes.
#[test]
fn plain_format_is_uncolored_when_redirected() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        assert!(
            !text.contains('\u{1b}'),
            "ANSI escape in a redirected log:\n{text}"
        );
        if text
            .lines()
            .any(|l| l.contains("INFO") && l.contains("rapira_core v"))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no plain banner line in server.log\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `[log.targets] php = "warn"` surfaces PHP diagnostics that `level = "error"` alone hides.
#[test]
fn log_targets_php_restores_php_diagnostics() {
    let srv = spawn_without_rust_log("logging/warn-worker.php", 1, "[log]\nlevel = \"error\"\n");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    std::thread::sleep(Duration::from_millis(300));
    let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
    assert!(
        !text.contains("WARN-MARK"),
        "php warning leaked past level = \"error\":\n{text}"
    );
    drop(srv);

    let srv = spawn_without_rust_log(
        "logging/warn-worker.php",
        1,
        "[log]\nlevel = \"error\"\n[log.targets]\nphp = \"warn\"\n",
    );
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        if text.contains("WARN-MARK") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "php = \"warn\" did not surface the diagnostic\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `RUST_LOG` replaces the configured filter wholesale, so `RUST_LOG=info` beats `level = "error"`.
#[test]
fn rust_log_replaces_the_config_filter() {
    let srv = spawn_with_config("logging/warn-worker.php", 1, "[log]\nlevel = \"error\"\n");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        if text.contains("WARN-MARK") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "RUST_LOG=info did not override level = \"error\"\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

const MARKS: &str = "logging_mv/marks-worker.php";
const BANNER: &str = "rapira_core v";

/// A RUST_LOG with a directive replaces the `[log]` filter spec; a missing or blank one leaves the spec in charge, and a `[log.targets]` key matches as a target prefix.
#[test]
fn log_filter_source_and_target_prefix() {
    struct Case {
        name: &'static str,
        /// None removes RUST_LOG from the environment.
        rust_log: Option<&'static str>,
        log: &'static str,
        shown: &'static [&'static str],
        hidden: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "config spec with a target prefix",
            rust_log: None,
            log: "[log]\nlevel = \"error\"\n[log.targets]\nph = \"warn\"\n",
            shown: &["WARN-MARK"],
            hidden: &["NOTICE-MARK", BANNER],
        },
        Case {
            name: "RUST_LOG replaces the whole config spec",
            rust_log: Some("info"),
            log: "[log]\nlevel = \"error\"\n[log.targets]\nphp = \"warn\"\n",
            shown: &["WARN-MARK", "NOTICE-MARK", BANNER],
            hidden: &[],
        },
        Case {
            name: "blank RUST_LOG falls back to the config spec",
            rust_log: Some("  "),
            log: "[log]\nlevel = \"error\"\n[log.targets]\nphp = \"warn\"\n",
            shown: &["WARN-MARK"],
            hidden: &["NOTICE-MARK", BANNER],
        },
        Case {
            name: "invalid RUST_LOG directive is dropped, the rest stays",
            rust_log: Some("!!!,warn"),
            log: "[log]\nlevel = \"trace\"\n",
            shown: &["WARN-MARK"],
            hidden: &["NOTICE-MARK", BANNER],
        },
    ];
    for case in cases {
        let mut srv = match case.rust_log {
            None => spawn_without_rust_log(MARKS, 1, case.log),
            Some(filter) => Spawn::http(Mode::Dispatcher, fixture_path(MARKS))
                .toml(case.log)
                .rust_log(filter)
                .spawn(),
        };
        let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
        assert_eq!(code, 200, "{}\n{}", case.name, diagnostics(&srv));
        srv.stop();
        let text = std::fs::read_to_string(srv.log_file()).expect("read server.log");
        for mark in case.shown {
            assert!(
                text.contains(mark),
                "{}: {mark} missing:\n{text}",
                case.name
            );
        }
        for mark in case.hidden {
            assert!(
                !text.contains(mark),
                "{}: {mark} leaked:\n{text}",
                case.name
            );
        }
    }
}

/// A JSON record nests the event fields under `fields`, names the target and the level, stamps UTC milliseconds and carries no span keys.
#[test]
fn json_record_shape() {
    let mut srv = Spawn::http(Mode::Dispatcher, fixture_path(MARKS))
        .json_log()
        .spawn();
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    srv.stop();
    let text = std::fs::read_to_string(srv.log_file()).expect("read server.log");
    let records: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| l.contains("APP-MARK"))
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert_eq!(records.len(), 1, "one JSON record per event:\n{text}");
    let v = &records[0];
    assert_eq!(v["fields"]["message"], "APP-MARK", "{v}");
    assert_eq!(v["fields"]["context"], r#"{"answer":42}"#, "{v}");
    assert_eq!(v["target"], "app", "{v}");
    assert_eq!(v["level"], "INFO", "{v}");
    // ChronoUtc %.3f: RFC 3339 UTC with exactly milliseconds.
    let ts = v["timestamp"].as_str().expect("timestamp");
    assert_eq!(ts.len(), "2026-01-01T00:00:00.000Z".len(), "{ts}");
    assert_eq!(&ts[19..20], ".", "{ts}");
    assert!(ts.ends_with('Z'), "{ts}");
    assert!(v.get("span").is_none() && v.get("spans").is_none(), "{v}");
}
