use std::fmt::Write as _;

use crate::stats::{PoolStats, STATES};

/// The versions for `rapira_build_info`.
#[derive(Debug)]
pub struct Build {
    /// The version of the `rapira` binary.
    pub version: &'static str,
    /// PHP_VERSION of the linked libphp.
    pub php_version: String,
}

/// Prometheus 3 fails a scrape without a known content type. See https://prometheus.io/docs/prometheus/latest/migration/#scrape-protocols
pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A family with one sample for each pool.
struct PoolFamily {
    name: &'static str,
    /// The metric type.
    kind: &'static str,
    help: &'static str,
    /// Gives the value of a pool.
    value: fn(&PoolStats) -> u64,
}

/// Writes the text format. Each family is one group, with the HELP and TYPE lines first. All label values are fixed ASCII strings, so nothing needs escaping. https://prometheus.io/docs/instrumenting/exposition_formats/#prometheus-text-format
pub(crate) fn render(pools: &[PoolStats], build: &Build) -> String {
    let mut out = String::new();

    family(&mut out, "rapira_workers", "gauge", "Workers per state.");
    for p in pools {
        for (s, &count) in STATES.iter().zip(&p.states) {
            sample(
                &mut out,
                "rapira_workers",
                &[("pool", p.name), ("state", s.label)],
                count,
            );
        }
    }

    let per_pool: [PoolFamily; 6] = [
        PoolFamily {
            name: "rapira_workers_configured",
            kind: "gauge",
            help: "Workers configured for the pool.",
            value: |p| p.configured as u64,
        },
        PoolFamily {
            name: "rapira_requests_total",
            kind: "counter",
            help: "Units of work that a worker handled or the host refused.",
            value: |p| p.requests,
        },
        PoolFamily {
            name: "rapira_requests_failed_total",
            kind: "counter",
            help: "Units of work that the host could not complete.",
            value: |p| p.failed,
        },
        PoolFamily {
            name: "rapira_requests_failed_on_full_queue_total",
            kind: "counter",
            help: "Units of work that found the pool queue full and never entered it.",
            value: |p| p.failed_on_full_queue,
        },
        PoolFamily {
            name: "rapira_requests_queued",
            kind: "gauge",
            help: "Units of work that wait for a worker in the pool.",
            value: |p| p.queued,
        },
        PoolFamily {
            name: "rapira_script_restarts_total",
            kind: "counter",
            help: "App recycles on worker threads.",
            value: |p| p.script_restarts,
        },
    ];
    for PoolFamily {
        name,
        kind,
        help,
        value,
    } in per_pool
    {
        family(&mut out, name, kind, help);
        for p in pools {
            sample(&mut out, name, &[("pool", p.name)], value(p));
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

    const ONE_POOL: &str = r#"# HELP rapira_workers Workers per state.
# TYPE rapira_workers gauge
rapira_workers{pool="http",state="starting"} 0
rapira_workers{pool="http",state="idle"} 1
rapira_workers{pool="http",state="active"} 1
rapira_workers{pool="http",state="draining"} 0
# HELP rapira_workers_configured Workers configured for the pool.
# TYPE rapira_workers_configured gauge
rapira_workers_configured{pool="http"} 2
# HELP rapira_requests_total Units of work that a worker handled or the host refused.
# TYPE rapira_requests_total counter
rapira_requests_total{pool="http"} 23
# HELP rapira_requests_failed_total Units of work that the host could not complete.
# TYPE rapira_requests_failed_total counter
rapira_requests_failed_total{pool="http"} 5
# HELP rapira_requests_failed_on_full_queue_total Units of work that found the pool queue full and never entered it.
# TYPE rapira_requests_failed_on_full_queue_total counter
rapira_requests_failed_on_full_queue_total{pool="http"} 4
# HELP rapira_requests_queued Units of work that wait for a worker in the pool.
# TYPE rapira_requests_queued gauge
rapira_requests_queued{pool="http"} 2
# HELP rapira_script_restarts_total App recycles on worker threads.
# TYPE rapira_script_restarts_total counter
rapira_script_restarts_total{pool="http"} 1
# HELP rapira_build_info The versions of rapira and of the linked PHP.
# TYPE rapira_build_info gauge
rapira_build_info{version="0.8.1",php_version="8.5.10"} 1
"#;

    const TWO_POOLS: &str = r#"# HELP rapira_workers Workers per state.
# TYPE rapira_workers gauge
rapira_workers{pool="http",state="starting"} 1
rapira_workers{pool="http",state="idle"} 0
rapira_workers{pool="http",state="active"} 0
rapira_workers{pool="http",state="draining"} 0
rapira_workers{pool="grpc",state="starting"} 1
rapira_workers{pool="grpc",state="idle"} 0
rapira_workers{pool="grpc",state="active"} 0
rapira_workers{pool="grpc",state="draining"} 0
# HELP rapira_workers_configured Workers configured for the pool.
# TYPE rapira_workers_configured gauge
rapira_workers_configured{pool="http"} 1
rapira_workers_configured{pool="grpc"} 1
# HELP rapira_requests_total Units of work that a worker handled or the host refused.
# TYPE rapira_requests_total counter
rapira_requests_total{pool="http"} 0
rapira_requests_total{pool="grpc"} 0
# HELP rapira_requests_failed_total Units of work that the host could not complete.
# TYPE rapira_requests_failed_total counter
rapira_requests_failed_total{pool="http"} 0
rapira_requests_failed_total{pool="grpc"} 0
# HELP rapira_requests_failed_on_full_queue_total Units of work that found the pool queue full and never entered it.
# TYPE rapira_requests_failed_on_full_queue_total counter
rapira_requests_failed_on_full_queue_total{pool="http"} 0
rapira_requests_failed_on_full_queue_total{pool="grpc"} 0
# HELP rapira_requests_queued Units of work that wait for a worker in the pool.
# TYPE rapira_requests_queued gauge
rapira_requests_queued{pool="http"} 0
rapira_requests_queued{pool="grpc"} 0
# HELP rapira_script_restarts_total App recycles on worker threads.
# TYPE rapira_script_restarts_total counter
rapira_script_restarts_total{pool="http"} 0
rapira_script_restarts_total{pool="grpc"} 0
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
                name: "one pool with idle and active workers",
                pools: vec![PoolStats {
                    name: "http",
                    configured: 2,
                    states: [0, 1, 1, 0],
                    requests: 23,
                    failed: 5,
                    failed_on_full_queue: 4,
                    queued: 2,
                    script_restarts: 1,
                }],
                want: ONE_POOL,
            },
            Case {
                name: "two pools with starting workers",
                pools: vec![
                    PoolStats {
                        name: "http",
                        configured: 1,
                        states: [1, 0, 0, 0],
                        ..Default::default()
                    },
                    PoolStats {
                        name: "grpc",
                        configured: 1,
                        states: [1, 0, 0, 0],
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
