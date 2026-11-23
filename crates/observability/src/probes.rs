use std::fmt::Write as _;

use hyper::StatusCode;
use rapira_sapi::pool::PoolSnapshot;

pub(crate) fn readyz(pools: &[PoolSnapshot]) -> (StatusCode, String) {
    let mut body = String::new();
    for pool in pools {
        if !pool.workers.iter().any(|worker| worker.state.serving()) {
            let _ = writeln!(body, "pool {}: no ready worker", pool.name);
        }
    }
    if body.is_empty() {
        (StatusCode::OK, "ok\n".to_owned())
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body)
    }
}

#[cfg(test)]
mod tests {
    use rapira_sapi::pool::{State, WorkerSnapshot};

    use super::*;

    fn pool(name: &'static str, states: [State; 2]) -> PoolSnapshot {
        let mut workers = Vec::new();
        for state in states {
            workers.push(WorkerSnapshot {
                state,
                handled: 0,
                errors: 0,
                recycles: 0,
                last_activity_ms: 0,
            });
        }
        PoolSnapshot {
            name,
            pending: 0,
            failed_on_full_queue: 0,
            refused: 0,
            workers,
        }
    }

    #[test]
    fn readyz_needs_a_serving_worker_in_each_pool() {
        struct Case {
            name: &'static str,
            http: [State; 2],
            grpc: [State; 2],
            want: (StatusCode, &'static str),
        }
        let cases = [
            Case {
                name: "an idle HTTP worker and an active gRPC worker",
                http: [State::Starting, State::Idle],
                grpc: [State::Active, State::Draining],
                want: (StatusCode::OK, "ok\n"),
            },
            Case {
                name: "the first pool has no serving worker",
                http: [State::Starting, State::Starting],
                grpc: [State::Idle, State::Active],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool http: no ready worker\n",
                ),
            },
            Case {
                name: "the second pool has no serving worker",
                http: [State::Active, State::Active],
                grpc: [State::Starting, State::Draining],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool grpc: no ready worker\n",
                ),
            },
            Case {
                name: "neither pool has a serving worker",
                http: [State::Draining, State::Starting],
                grpc: [State::Starting, State::Draining],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool http: no ready worker\npool grpc: no ready worker\n",
                ),
            },
        ];
        for case in cases {
            let pools = [pool("http", case.http), pool("grpc", case.grpc)];
            let (status, body) = readyz(&pools);
            assert_eq!((status, body.as_str()), case.want, "{}", case.name);
        }
    }
}
