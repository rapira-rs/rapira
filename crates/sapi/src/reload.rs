use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

unsafe extern "C" {
    fn rapira_opcache_enabled() -> bool;
}

static SENT: AtomicU64 = AtomicU64::new(0);
static RESET: AtomicU64 = AtomicU64::new(0);
static RESETTING: Mutex<bool> = Mutex::new(false);
static RESET_DONE: Condvar = Condvar::new();

pub(crate) fn request_reset() {
    SENT.fetch_add(1, Ordering::Release);
}

fn reset_due() -> Option<u64> {
    let sent = SENT.load(Ordering::Acquire);
    (sent > RESET.load(Ordering::Acquire)).then_some(sent)
}

/// One worker resets the cache for all reloads through the returned sequence number. Other workers wait before they can load a script from the cache.
pub(crate) fn claim_reset() -> Option<u64> {
    reset_due()?;
    let mut active = RESET_DONE
        .wait_while(RESETTING.lock().unwrap(), |active| *active)
        .unwrap();
    let sent = reset_due()?;
    *active = true;
    Some(sent)
}

/// A failed request startup leaves the reset due for the next worker.
pub(crate) fn finish_reset(sent: u64, started: bool) {
    let mut active = RESETTING.lock().unwrap();
    if started {
        RESET.store(sent, Ordering::Release);
    }
    *active = false;
    RESET_DONE.notify_all();
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
