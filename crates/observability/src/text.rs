use std::fmt::Write as _;

use crate::memory::Memory;
use crate::stats::{EXIT_REASONS, PoolStats, STATES};

/// The versions for `rapira_build_info`.
#[derive(Debug)]
pub struct Build {
    /// The version of the `rapira` binary.
    pub version: &'static str,
    /// major.minor.patch of the linked libphp.
    pub php_version: String,
}

/// Prometheus 3 fails a scrape without a known content type: https://prometheus.io/docs/prometheus/latest/migration/#scrape-protocols
pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A family with one sample for each pool: the name, the type, the help text and the function that gives the value of a pool.
type PoolFamily = (
    &'static str,
    &'static str,
    &'static str,
    fn(&PoolStats) -> u64,
);

/// A memory family: the name, the help text and the function that gives the value from the reading of a worker.
type MemoryFamily = (&'static str, &'static str, fn(&Memory) -> Option<u64>);

/// The text format: each family is one group, with the HELP and TYPE lines first. All label values are fixed ASCII strings, so nothing needs escaping. https://prometheus.io/docs/instrumenting/exposition_formats/#prometheus-text-format
pub(crate) fn render(pools: &[PoolStats], build: &Build) -> String {
    let mut out = String::new();

    family(&mut out, "rapira_workers", "gauge", "Workers per state.");
    for p in pools {
        for (&(_, state), &count) in STATES.iter().zip(&p.states) {
            sample(
                &mut out,
                "rapira_workers",
                &[("pool", p.name), ("state", state)],
                count,
            );
        }
    }

    let per_pool: [PoolFamily; 6] = [
        (
            "rapira_workers_configured",
            "gauge",
            "The processes value of the pool config.",
            |p| p.configured as u64,
        ),
        (
            "rapira_requests_total",
            "counter",
            "Units of work that the workers finished in any way.",
            |p| p.requests,
        ),
        (
            "rapira_requests_failed_total",
            "counter",
            "Units of work that the host could not complete.",
            |p| p.failed,
        ),
        (
            "rapira_requests_failed_on_full_queue_total",
            "counter",
            "Units of work that found the worker queue full and never entered it.",
            |p| p.failed_on_full_queue,
        ),
        (
            "rapira_requests_queued",
            "gauge",
            "Units of work that wait for the PHP thread.",
            |p| p.queued,
        ),
        (
            "rapira_script_restarts_total",
            "counter",
            "Entrypoint restarts inside a worker process.",
            |p| p.script_restarts,
        ),
    ];
    for (name, kind, help, value) in per_pool {
        family(&mut out, name, kind, help);
        for p in pools {
            sample(&mut out, name, &[("pool", p.name)], value(p));
        }
    }

    family(
        &mut out,
        "rapira_worker_exits_total",
        "counter",
        "Worker process exits.",
    );
    for p in pools {
        for (&reason, &count) in EXIT_REASONS.iter().zip(&p.exits) {
            sample(
                &mut out,
                "rapira_worker_exits_total",
                &[("pool", p.name), ("reason", reason)],
                count,
            );
        }
    }

    let memory: [MemoryFamily; 2] = [
        (
            "rapira_worker_rss_bytes",
            "Resident memory of a worker in bytes.",
            |m| m.rss,
        ),
        (
            "rapira_worker_pss_bytes",
            "Proportional memory of a worker in bytes.",
            |m| m.pss,
        ),
    ];
    for (name, help, value) in memory {
        family(&mut out, name, "gauge", help);
        for p in pools {
            for w in &p.workers {
                if let Some(bytes) = value(&w.memory) {
                    let worker = w.index.to_string();
                    sample(
                        &mut out,
                        name,
                        &[("pool", p.name), ("worker", worker.as_str())],
                        bytes,
                    );
                }
            }
        }
    }

    family(
        &mut out,
        "rapira_build_info",
        "gauge",
        "The versions of rapira and of the linked PHP.",
    );
    sample(
        &mut out,
        "rapira_build_info",
        &[
            ("version", build.version),
            ("php_version", build.php_version.as_str()),
        ],
        1,
    );
    out
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: u64) {
    out.push_str(name);
    out.push('{');
    for (i, (key, val)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{key}=\"{val}\"");
    }
    let _ = writeln!(out, "}} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Worker;

    const ONE_POOL: &str = r#"# HELP rapira_workers Workers per state.
# TYPE rapira_workers gauge
rapira_workers{pool="http",state="starting"} 0
rapira_workers{pool="http",state="idle"} 1
rapira_workers{pool="http",state="active"} 1
rapira_workers{pool="http",state="draining"} 0
# HELP rapira_workers_configured The processes value of the pool config.
# TYPE rapira_workers_configured gauge
rapira_workers_configured{pool="http"} 2
# HELP rapira_requests_total Units of work that the workers finished in any way.
# TYPE rapira_requests_total counter
rapira_requests_total{pool="http"} 23
# HELP rapira_requests_failed_total Units of work that the host could not complete.
# TYPE rapira_requests_failed_total counter
rapira_requests_failed_total{pool="http"} 3
# HELP rapira_requests_failed_on_full_queue_total Units of work that found the worker queue full and never entered it.
# TYPE rapira_requests_failed_on_full_queue_total counter
rapira_requests_failed_on_full_queue_total{pool="http"} 4
# HELP rapira_requests_queued Units of work that wait for the PHP thread.
# TYPE rapira_requests_queued gauge
rapira_requests_queued{pool="http"} 2
# HELP rapira_script_restarts_total Entrypoint restarts inside a worker process.
# TYPE rapira_script_restarts_total counter
rapira_script_restarts_total{pool="http"} 1
# HELP rapira_worker_exits_total Worker process exits.
# TYPE rapira_worker_exits_total counter
rapira_worker_exits_total{pool="http",reason="drained"} 1
rapira_worker_exits_total{pool="http",reason="recycled"} 2
rapira_worker_exits_total{pool="http",reason="unhealthy"} 0
rapira_worker_exits_total{pool="http",reason="timeout"} 0
rapira_worker_exits_total{pool="http",reason="crashed"} 1
# HELP rapira_worker_rss_bytes Resident memory of a worker in bytes.
# TYPE rapira_worker_rss_bytes gauge
rapira_worker_rss_bytes{pool="http",worker="0"} 4096
# HELP rapira_worker_pss_bytes Proportional memory of a worker in bytes.
# TYPE rapira_worker_pss_bytes gauge
rapira_worker_pss_bytes{pool="http",worker="0"} 2048
# HELP rapira_build_info The versions of rapira and of the linked PHP.
# TYPE rapira_build_info gauge
rapira_build_info{version="0.8.1",php_version="8.5.10"} 1
"#;

    const TWO_POOLS: &str = r#"# HELP rapira_workers Workers per state.
# TYPE rapira_workers gauge
rapira_workers{pool="http",state="starting"} 0
rapira_workers{pool="http",state="idle"} 0
rapira_workers{pool="http",state="active"} 0
rapira_workers{pool="http",state="draining"} 0
rapira_workers{pool="grpc",state="starting"} 0
rapira_workers{pool="grpc",state="idle"} 0
rapira_workers{pool="grpc",state="active"} 0
rapira_workers{pool="grpc",state="draining"} 0
# HELP rapira_workers_configured The processes value of the pool config.
# TYPE rapira_workers_configured gauge
rapira_workers_configured{pool="http"} 1
rapira_workers_configured{pool="grpc"} 1
# HELP rapira_requests_total Units of work that the workers finished in any way.
# TYPE rapira_requests_total counter
rapira_requests_total{pool="http"} 0
rapira_requests_total{pool="grpc"} 0
# HELP rapira_requests_failed_total Units of work that the host could not complete.
# TYPE rapira_requests_failed_total counter
rapira_requests_failed_total{pool="http"} 0
rapira_requests_failed_total{pool="grpc"} 0
# HELP rapira_requests_failed_on_full_queue_total Units of work that found the worker queue full and never entered it.
# TYPE rapira_requests_failed_on_full_queue_total counter
rapira_requests_failed_on_full_queue_total{pool="http"} 0
rapira_requests_failed_on_full_queue_total{pool="grpc"} 0
# HELP rapira_requests_queued Units of work that wait for the PHP thread.
# TYPE rapira_requests_queued gauge
rapira_requests_queued{pool="http"} 0
rapira_requests_queued{pool="grpc"} 0
# HELP rapira_script_restarts_total Entrypoint restarts inside a worker process.
# TYPE rapira_script_restarts_total counter
rapira_script_restarts_total{pool="http"} 0
rapira_script_restarts_total{pool="grpc"} 0
# HELP rapira_worker_exits_total Worker process exits.
# TYPE rapira_worker_exits_total counter
rapira_worker_exits_total{pool="http",reason="drained"} 0
rapira_worker_exits_total{pool="http",reason="recycled"} 0
rapira_worker_exits_total{pool="http",reason="unhealthy"} 0
rapira_worker_exits_total{pool="http",reason="timeout"} 0
rapira_worker_exits_total{pool="http",reason="crashed"} 0
rapira_worker_exits_total{pool="grpc",reason="drained"} 0
rapira_worker_exits_total{pool="grpc",reason="recycled"} 0
rapira_worker_exits_total{pool="grpc",reason="unhealthy"} 0
rapira_worker_exits_total{pool="grpc",reason="timeout"} 0
rapira_worker_exits_total{pool="grpc",reason="crashed"} 0
# HELP rapira_worker_rss_bytes Resident memory of a worker in bytes.
# TYPE rapira_worker_rss_bytes gauge
# HELP rapira_worker_pss_bytes Proportional memory of a worker in bytes.
# TYPE rapira_worker_pss_bytes gauge
# HELP rapira_build_info The versions of rapira and of the linked PHP.
# TYPE rapira_build_info gauge
rapira_build_info{version="0.8.1",php_version="8.5.10"} 1
"#;

    #[test]
    fn render_writes_every_family_as_one_group() {
        struct Case {
            name: &'static str,
            pools: Vec<PoolStats>,
            want: &'static str,
        }
        let cases = [
            Case {
                name: "one pool with a measured and an unmeasured worker",
                pools: vec![PoolStats {
                    name: "http",
                    configured: 2,
                    states: [0, 1, 1, 0],
                    requests: 23,
                    failed: 3,
                    failed_on_full_queue: 4,
                    queued: 2,
                    script_restarts: 1,
                    exits: [1, 2, 0, 0, 1],
                    workers: vec![
                        Worker {
                            index: 0,
                            pid: 201,
                            memory: Memory {
                                rss: Some(4096),
                                pss: Some(2048),
                            },
                        },
                        Worker {
                            index: 1,
                            pid: 202,
                            memory: Memory::default(),
                        },
                    ],
                }],
                want: ONE_POOL,
            },
            Case {
                name: "two pools without workers",
                pools: vec![
                    PoolStats {
                        name: "http",
                        configured: 1,
                        ..Default::default()
                    },
                    PoolStats {
                        name: "grpc",
                        configured: 1,
                        ..Default::default()
                    },
                ],
                want: TWO_POOLS,
            },
        ];
        let build = Build {
            version: "0.8.1",
            php_version: "8.5.10".to_owned(),
        };
        for case in cases {
            assert_eq!(render(&case.pools, &build), case.want, "{}", case.name);
        }
    }
}
