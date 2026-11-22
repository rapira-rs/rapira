use std::fmt::Write as _;
use std::sync::Arc;

use hyper::StatusCode;
use rapira_scoreboard::Pool;

pub(crate) fn readyz(board: &[Arc<Pool>]) -> (StatusCode, String) {
    let mut body = String::new();
    for pool in board.iter().filter(|pool| !pool.ready()) {
        let _ = writeln!(body, "pool {}: no ready worker", pool.name);
    }
    if body.is_empty() {
        (StatusCode::OK, "ok\n".to_owned())
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING};

    use super::*;

    #[test]
    fn readyz_needs_a_serving_worker_in_each_pool() {
        struct Case {
            name: &'static str,
            http: [u32; 2],
            grpc: [u32; 2],
            want: (StatusCode, &'static str),
        }
        let cases = [
            Case {
                name: "an idle HTTP worker and an active gRPC worker",
                http: [SLOT_STARTING, SLOT_IDLE],
                grpc: [SLOT_ACTIVE, SLOT_DRAINING],
                want: (StatusCode::OK, "ok\n"),
            },
            Case {
                name: "the first pool has no serving worker",
                http: [SLOT_STARTING, SLOT_STARTING],
                grpc: [SLOT_IDLE, SLOT_ACTIVE],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool http: no ready worker\n",
                ),
            },
            Case {
                name: "the second pool has no serving worker",
                http: [SLOT_ACTIVE, SLOT_ACTIVE],
                grpc: [SLOT_STARTING, SLOT_DRAINING],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool grpc: no ready worker\n",
                ),
            },
            Case {
                name: "both pools served before and now have no serving workers",
                http: [SLOT_DRAINING, SLOT_STARTING],
                grpc: [SLOT_STARTING, SLOT_DRAINING],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool http: no ready worker\npool grpc: no ready worker\n",
                ),
            },
        ];
        let board = [
            Arc::new(Pool::new("http", 2)),
            Arc::new(Pool::new("grpc", 2)),
        ];
        for pool in &board {
            pool.served.store(true, Relaxed);
        }
        for case in cases {
            for (pool, states) in board.iter().zip([case.http, case.grpc]) {
                for (worker, state) in pool.workers.iter().zip(states) {
                    worker.state.store(state, Relaxed);
                }
            }
            let (status, body) = readyz(&board);
            assert_eq!((status, body.as_str()), case.want, "{}", case.name);
        }
    }
}
