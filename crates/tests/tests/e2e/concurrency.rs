use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::harness::{Server, diagnostics, http_get, spawn_with_config, wait_workers};

const REQ: Duration = Duration::from_secs(10);

/// Concurrent producers over a worker pool must never get another request's body.
#[test]
fn concurrent_unique_requests_never_mix() {
    let srv = spawn_with_config("lifecycle/fiber-worker.php", 4, "");
    wait_workers(&srv, Duration::from_secs(20), "4 workers", |p| p.len() == 4);

    let addr = srv.addr;
    let producers: Vec<_> = (0..8u32)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..50u32 {
                    let n = t * 1000 + i;
                    let (code, body) = http_get(addr, &format!("/?n={n}"), Duration::from_secs(10))
                        .unwrap_or_else(|e| panic!("GET /?n={n}: {e}"));
                    assert_eq!(code, 200, "n={n}");
                    assert_eq!(
                        String::from_utf8_lossy(&body),
                        format!("r={}", n + 1),
                        "response must answer exactly its own request (n={n})"
                    );
                }
            })
        })
        .collect();
    for p in producers {
        if let Err(e) = p.join() {
            std::panic::resume_unwind(e);
        }
    }
}

fn serving_pid(srv: &Server) -> u32 {
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(srv));
    let body = String::from_utf8_lossy(&body);
    body.strip_prefix("ok:")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("unexpected body {body:?}"))
}

/// One request at a time leaves every other worker blocked in accept when the next
/// connection arrives. The kernel wakes one of them, and that worker re-registers behind
/// the others, so the connections go round the pool.
#[test]
#[cfg(target_os = "linux")]
fn sequential_requests_go_round_the_pool() {
    let srv = spawn_with_config("shared/echo-worker.php", 4, "");
    let workers = wait_workers(&srv, Duration::from_secs(20), "4 workers", |p| p.len() == 4);

    // A worker joins the queue when it first blocks in accept, so let every one answer.
    let mut warm = HashSet::new();
    for _ in 0..40 {
        warm.insert(serving_pid(&srv));
        if warm.len() == workers.len() {
            break;
        }
    }
    assert_eq!(
        warm.len(),
        workers.len(),
        "every worker must answer before the split is measured\n{}",
        diagnostics(&srv)
    );

    let mut served: HashMap<u32, usize> = HashMap::new();
    for _ in 0..8 {
        *served.entry(serving_pid(&srv)).or_default() += 1;
    }
    let mut shares: Vec<usize> = served.values().copied().collect();
    shares.sort_unstable();
    assert_eq!(
        shares,
        vec![2, 2, 2, 2],
        "each worker must take every fourth connection: {served:?}\n{}",
        diagnostics(&srv)
    );
}
