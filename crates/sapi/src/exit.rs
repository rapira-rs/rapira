//! Process-wide cleanup hooks executed right before an abnormal `libc::_exit`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

type CleanupFn = Box<dyn FnOnce() + Send>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CLEANUPS: Mutex<Vec<(u64, CleanupFn)>> = Mutex::new(Vec::new());

/// Deregisters an abnormal-exit cleanup hook on normal drop.
pub struct CleanupGuard {
    id: u64,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let mut list = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = list.iter().position(|(id, _)| *id == self.id) {
            let _ = list.remove(pos);
        }
    }
}

/// Registers a cleanup closure to run if a thread calls [`exit`].
/// Drop the returned guard during normal teardown to deregister the hook.
pub fn register(f: impl FnOnce() + Send + 'static) -> CleanupGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut list = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
    list.push((id, Box::new(f)));
    CleanupGuard { id }
}

fn run_cleanups(list: &mut Vec<(u64, CleanupFn)>) {
    // Run in reverse registration order (LIFO), catching panics so all hooks run.
    while let Some((_, f)) = list.pop() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }
}

/// Runs all registered abnormal-exit cleanup hooks and terminates the process with `libc::_exit`.
/// Holds the registry lock until `_exit` so concurrent failing threads wait for cleanup to finish.
pub fn exit(code: i32) -> ! {
    let mut list = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
    run_cleanups(&mut list);
    // SAFETY: Caller requests immediate process termination without running C/Rust atexit destructors.
    unsafe { libc::_exit(code) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn cleanups_run_in_lifo_order_and_ignore_panics() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut list: Vec<(u64, CleanupFn)> = Vec::new();
        let l1 = Arc::clone(&log);
        list.push((1, Box::new(move || l1.lock().unwrap().push("first"))));
        list.push((2, Box::new(|| panic!("boom"))));
        let l3 = Arc::clone(&log);
        list.push((3, Box::new(move || l3.lock().unwrap().push("third"))));

        run_cleanups(&mut list);
        assert!(list.is_empty());
        assert_eq!(*log.lock().unwrap(), vec!["third", "first"]);
    }

    #[test]
    fn guard_drop_removes_only_its_own_registration() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let g1 = register(|| {});
        let g2 = register(|| {});
        let id1 = g1.id;
        let id2 = g2.id;
        drop(g1);
        {
            let list = CLEANUPS.lock().unwrap();
            assert!(!list.iter().any(|(id, _)| *id == id1));
            assert!(list.iter().any(|(id, _)| *id == id2));
        }
        drop(g2);
        let list = CLEANUPS.lock().unwrap();
        assert!(!list.iter().any(|(id, _)| *id == id2));
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn guard_drop_preserves_lifo_order_for_remaining_hooks() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        struct Case {
            name: &'static str,
            drop_index: usize,
            want_order: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "dropping first of three preserves C before B",
                drop_index: 0,
                want_order: &["C", "B"],
            },
            Case {
                name: "dropping middle of three preserves C before A",
                drop_index: 1,
                want_order: &["C", "A"],
            },
            Case {
                name: "dropping last of three preserves B before A",
                drop_index: 2,
                want_order: &["B", "A"],
            },
        ];

        for case in cases {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut guards: Vec<Option<CleanupGuard>> = ["A", "B", "C"]
                .into_iter()
                .map(|label| {
                    let l = Arc::clone(&log);
                    Some(register(move || l.lock().unwrap().push(label)))
                })
                .collect();
            drop(guards[case.drop_index].take());
            {
                let mut list = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
                run_cleanups(&mut list);
            }
            for g in guards.into_iter().flatten() {
                std::mem::forget(g);
            }
            assert_eq!(*log.lock().unwrap(), case.want_order, "{}", case.name);
        }
    }
}
