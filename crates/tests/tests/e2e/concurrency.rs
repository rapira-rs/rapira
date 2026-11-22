use std::time::Duration;

use rapira_sapi::plugin::Mode;
use tests::StatusBody;

use crate::harness::{Spawn, fixture_path, http_get, is_zts, wait_serving};

/// Concurrent producers over a worker pool must never get another request's body.
#[test]
fn concurrent_unique_requests_never_mix() {
    let workers = if is_zts() { 4 } else { 1 };
    let srv = Spawn::http(Mode::Dispatcher, fixture_path("lifecycle/fiber-worker.php"))
        .workers(workers)
        .spawn();
    wait_serving(&srv, "http", workers);

    let addr = srv.addr;
    let producers: Vec<_> = (0..8u32)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..50u32 {
                    let n = t * 1000 + i;
                    let StatusBody { status: code, body } =
                        http_get(addr, &format!("/?n={n}"), Duration::from_secs(10))
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
