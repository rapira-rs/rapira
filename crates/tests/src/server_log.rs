//! The records of a spawned server's log in JSON format (`[log] format = "json"`), for the assertions that the in-process capture served.
//!
//! A record line is `{"level", "target", "fields": {"message", "context"}}`. The RUST_LOG filter of the server decides which records the log holds.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{AppRecord, Captured, result_text};

/// The records of `log` in file order. A line that is not a JSON record is skipped, for example a line that the server still writes.
pub fn records(log: &Path) -> Vec<Captured> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines().filter_map(record).collect()
}

fn record(line: &str) -> Option<Captured> {
    let v: Value = serde_json::from_str(line).ok()?;
    let field = |name: &str| v["fields"][name].as_str().unwrap_or_default().to_owned();
    Some(Captured {
        level: v["level"].as_str()?.parse().ok()?,
        target: v["target"].as_str()?.to_owned(),
        message: field("message"),
        context: field("context"),
    })
}

/// The `app` records of `log` and the messages of its `php` records.
pub fn app_records(log: &Path) -> (Vec<AppRecord>, Vec<String>) {
    let all = records(log);
    let app = all
        .iter()
        .filter(|c| c.target == "app")
        .map(|c| (c.level, c.message.clone(), c.context.clone()))
        .collect();
    let php = all
        .into_iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message)
        .collect();
    (app, php)
}

/// The one `app` record of `log`; a stray extra record fails the test.
pub fn app_record(log: &Path) -> AppRecord {
    let (records, _) = app_records(log);
    assert_eq!(
        records.len(),
        1,
        "{} must hold exactly one app record (got {records:?})",
        log.display()
    );
    records.into_iter().next().expect("checked above")
}

/// Polls `log` until an `app` record with `message` appears, for at most 10 s; returns its context.
pub fn wait_app_record(log: &Path, message: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(c) = records(log)
            .into_iter()
            .find(|c| c.target == "app" && c.message == message)
        {
            return c.context;
        }
        assert!(
            Instant::now() < deadline,
            "no {message:?} app record within 10s in {}",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The context of the one `dispatcher` app record of `log`. Stop the server first, so the worker has run to its end.
pub fn dispatcher_record(log: &Path) -> anyhow::Result<Value> {
    let all = records(log);
    let records: Vec<&str> = all
        .iter()
        .filter(|c| c.target == "app" && c.message == "dispatcher")
        .map(|c| c.context.as_str())
        .collect();
    let php: Vec<&str> = all
        .iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message.as_str())
        .collect();
    assert_eq!(
        records.len(),
        1,
        "one dispatcher record (got {records:?}, php: {php:?})"
    );
    Ok(serde_json::from_str(records[0])?)
}

/// The `result` field of each app record named `message`, in log order.
pub fn app_results(log: &Path, message: &str) -> Vec<String> {
    records(log)
        .iter()
        .filter(|c| c.target == "app" && c.message == message)
        .filter_map(|c| serde_json::from_str::<Value>(&c.context).ok())
        .map(|ctx| result_text(&ctx))
        .collect()
}

/// A fixture that probes one case at a time logs `case {name, result}`; this is name to result.
pub fn case_results(log: &Path) -> HashMap<String, String> {
    case_map(&records(log))
}

fn case_map(all: &[Captured]) -> HashMap<String, String> {
    all.iter()
        .filter(|c| c.target == "app" && c.message == "case")
        .filter_map(|c| serde_json::from_str::<Value>(&c.context).ok())
        .map(|ctx| {
            let name = ctx["name"].as_str().unwrap_or_default().to_owned();
            (name, result_text(&ctx))
        })
        .collect()
}

/// One `case` record per (name, expected result) row, and no stray one; every mismatch is listed at once.
pub fn assert_case_records(log: &Path, cases: &[(&str, &str)]) {
    let all = records(log);
    let count = all
        .iter()
        .filter(|c| c.target == "app" && c.message == "case")
        .count();
    let results = case_map(&all);
    assert_eq!(count, cases.len(), "one record per case (got {results:?})");
    let mismatches: Vec<String> = cases
        .iter()
        .filter(|(name, expected)| results.get(*name).map(String::as_str) != Some(*expected))
        .map(|(name, expected)| {
            format!(
                "{name}: expected {expected:?}, got {:?}",
                results.get(*name)
            )
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}
