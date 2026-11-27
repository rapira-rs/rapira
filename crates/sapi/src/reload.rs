use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

unsafe extern "C" {
    fn rapira_opcache_enabled() -> bool;
}

/// Process-wide coordinator ensuring one worker resets OPcache per reload generation while peers wait.
#[derive(Default)]
pub(crate) struct OpcacheCoordinator {
    sent: AtomicU64,
    reset: AtomicU64,
    resetting: Mutex<bool>,
    reset_done: Condvar,
}

impl OpcacheCoordinator {
    pub(crate) fn request_reset(&self) {
        self.sent.fetch_add(1, Ordering::Release);
    }

    fn reset_due(&self) -> Option<u64> {
        let sent = self.sent.load(Ordering::Acquire);
        (sent > self.reset.load(Ordering::Acquire)).then_some(sent)
    }

    /// One worker resets the cache for all reloads through the returned sequence number. Other workers wait before loading a script.
    pub(crate) fn claim_reset(&self) -> Option<u64> {
        self.reset_due()?;
        let mut active = self
            .reset_done
            .wait_while(self.resetting.lock().unwrap(), |active| *active)
            .unwrap();
        let sent = self.reset_due()?;
        *active = true;
        Some(sent)
    }

    /// A failed request startup leaves the reset due for the next worker.
    pub(crate) fn finish_reset(&self, sent: u64, started: bool) {
        let mut active = self.resetting.lock().unwrap();
        if started {
            self.reset.store(sent, Ordering::Release);
        }
        *active = false;
        self.reset_done.notify_all();
    }
}

/// Call on the main thread after module startup and before the workers start.
pub(crate) fn check_opcache(total_workers: usize) -> anyhow::Result<()> {
    // Both calls read startup state. They need no PHP request.
    let (version, enabled) = unsafe { (crate::php_version_id(), rapira_opcache_enabled()) };
    if let Some(needed) = missing_opcache_fix(version, enabled, total_workers) {
        anyhow::bail!(
            "PHP {} cannot run {total_workers} workers with OPcache. Use PHP {needed} or later in this minor.",
            crate::linked_php_version()
        );
    }
    Ok(())
}

// The fix protects shared memory while other threads use it. https://github.com/php/php-src/commit/36c6d1f500f7ec477f6eae0b0f5c46b041d82347
fn missing_opcache_fix(version: u32, enabled: bool, total_workers: usize) -> Option<&'static str> {
    if !enabled || total_workers <= 1 {
        return None;
    }
    match version {
        80400..80429 => Some("8.4.29"),
        80500..80514 => Some("8.5.14"),
        80600 => Some("8.6.1"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcache_coordinator_serializes_resets_and_retries_failures() {
        struct Case {
            name: &'static str,
            first_started: bool,
            want_second_claim: bool,
        }
        let cases = [
            Case {
                name: "successful reset satisfies pending generation for later callers",
                first_started: true,
                want_second_claim: false,
            },
            Case {
                name: "failed reset leaves generation pending for next worker",
                first_started: false,
                want_second_claim: true,
            },
        ];
        for case in cases {
            let coord = OpcacheCoordinator::default();
            assert_eq!(coord.claim_reset(), None, "{}", case.name);
            coord.request_reset();
            let sent = coord.claim_reset().expect("first claim");
            assert_eq!(sent, 1, "{}", case.name);
            coord.finish_reset(sent, case.first_started);
            assert_eq!(
                coord.claim_reset().is_some(),
                case.want_second_claim,
                "{}",
                case.name
            );
            if case.want_second_claim {
                coord.finish_reset(sent, true);
            }
        }
    }

    #[test]
    fn opcache_version_for_several_workers() {
        struct Case {
            name: &'static str,
            version: u32,
            enabled: bool,
            workers: usize,
            required: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "OPcache is off",
                version: 80513,
                enabled: false,
                workers: 2,
                required: None,
            },
            Case {
                name: "one worker",
                version: 80600,
                enabled: true,
                workers: 1,
                required: None,
            },
            Case {
                name: "8.4 before the fix",
                version: 80428,
                enabled: true,
                workers: 2,
                required: Some("8.4.29"),
            },
            Case {
                name: "8.4 with the fix",
                version: 80429,
                enabled: true,
                workers: 2,
                required: None,
            },
            Case {
                name: "8.5 before the fix",
                version: 80513,
                enabled: true,
                workers: 2,
                required: Some("8.5.14"),
            },
            Case {
                name: "8.5 with the fix",
                version: 80514,
                enabled: true,
                workers: 2,
                required: None,
            },
            Case {
                name: "8.6 before the fix",
                version: 80600,
                enabled: true,
                workers: 2,
                required: Some("8.6.1"),
            },
            Case {
                name: "8.6 with the fix",
                version: 80601,
                enabled: true,
                workers: 2,
                required: None,
            },
            Case {
                name: "8.7 includes the fix",
                version: 80700,
                enabled: true,
                workers: 2,
                required: None,
            },
        ];
        for case in cases {
            assert_eq!(
                missing_opcache_fix(case.version, case.enabled, case.workers),
                case.required,
                "{}",
                case.name
            );
        }
    }
}
