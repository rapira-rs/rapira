use crate::{
    E_COMPILE_WARNING, E_CORE, E_CORE_WARNING, E_DEPRECATED, E_FATAL_ERRORS, E_NOTICE,
    E_USER_DEPRECATED, E_USER_NOTICE, E_USER_WARNING, E_WARNING,
};
use std::os::raw::c_int;

// php-src groups the fatal and core types itself; these three have no upstream equivalent.
const WARNINGS: u32 = E_WARNING | E_CORE_WARNING | E_COMPILE_WARNING | E_USER_WARNING;
const NOTICES: u32 = E_NOTICE | E_USER_NOTICE;
const DEPRECATIONS: u32 = E_DEPRECATED | E_USER_DEPRECATED;

/// `tracing::event!` needs a const level and target, so the runtime level fans out over five arms.
macro_rules! php_log {
    ($lvl:expr, $($arg:tt)+) => {
        match $lvl {
            tracing::Level::ERROR => tracing::event!(target: "php", tracing::Level::ERROR, $($arg)+),
            tracing::Level::WARN => tracing::event!(target: "php", tracing::Level::WARN, $($arg)+),
            tracing::Level::INFO => tracing::event!(target: "php", tracing::Level::INFO, $($arg)+),
            tracing::Level::DEBUG => tracing::event!(target: "php", tracing::Level::DEBUG, $($arg)+),
            tracing::Level::TRACE => tracing::event!(target: "php", tracing::Level::TRACE, $($arg)+),
        }
    };
}
pub(crate) use php_log;

/// A masked non-fatal drops to `Trace` rather than vanishing; fatals ignore the mask. https://www.php.net/manual/en/function.error-reporting.php
pub(crate) fn error_type_to_level(err_type: c_int, mask: c_int) -> (tracing::Level, &'static str) {
    let (err_type, mask) = (err_type as u32, mask as u32);
    let (level, label) = match err_type {
        t if t & E_FATAL_ERRORS != 0 => (tracing::Level::ERROR, "Fatal error"),
        t if t & WARNINGS != 0 => (tracing::Level::WARN, "Warning"),
        t if t & NOTICES != 0 => (tracing::Level::INFO, "Notice"),
        t if t & DEPRECATIONS != 0 => (tracing::Level::DEBUG, "Deprecated"),
        _ => (tracing::Level::WARN, "Unknown error"),
    };
    if err_type != 0 && err_type & E_FATAL_ERRORS == 0 && err_type & (mask | E_CORE) == 0 {
        return (tracing::Level::TRACE, label);
    }
    (level, label)
}

/// Syslog priorities LOG_EMERG(0)..LOG_DEBUG(7); php-src reports deprecations at LOG_INFO (main/main.c:1443-1446).
pub(crate) fn syslog_to_level(syslog_lev: c_int) -> tracing::Level {
    match syslog_lev {
        0 => tracing::Level::ERROR,
        1 => tracing::Level::ERROR,
        2 => tracing::Level::ERROR,
        3 => tracing::Level::ERROR,
        4 => tracing::Level::WARN,
        5 => tracing::Level::INFO,
        6 => tracing::Level::DEBUG,
        7 => tracing::Level::DEBUG,
        _ => tracing::Level::INFO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    // E_ERROR is outside the bindgen allowlist; it is the lowest bit of php-src's fatal group.
    const E_ERROR: u32 = 1 << E_FATAL_ERRORS.trailing_zeros();

    fn level_of(err_type: u32, mask: u32) -> (Level, &'static str) {
        error_type_to_level(err_type as c_int, mask as c_int)
    }

    /// The fatal arm matches before the warning arm, and the mask clause skips fatals.
    #[test]
    fn fatals_outrank_a_warning_bit_and_ignore_the_mask() {
        assert_eq!(
            level_of(E_ERROR | E_WARNING, E_WARNING),
            (Level::ERROR, "Fatal error")
        );
        assert_eq!(level_of(E_ERROR, 0), (Level::ERROR, "Fatal error"));
    }

    /// A masked non-fatal drops to `Trace` with its label intact.
    #[test]
    fn a_masked_warning_drops_to_trace_with_its_label() {
        assert_eq!(level_of(E_USER_WARNING, 0), (Level::TRACE, "Warning"));
        assert_eq!(
            level_of(E_USER_WARNING, E_USER_WARNING),
            (Level::WARN, "Warning")
        );
    }

    /// `mask | E_CORE` exempts core diagnostics, which the sampled `EG(error_reporting)` does not describe.
    #[test]
    fn core_warnings_survive_an_empty_mask() {
        assert_eq!(level_of(E_CORE_WARNING, 0), (Level::WARN, "Warning"));
    }

    /// A type no arm recognizes reports at `Warn` and is still subject to the mask.
    #[test]
    fn an_unknown_error_type_still_obeys_the_mask() {
        let unknown = 1 << 20;
        assert_eq!(level_of(unknown, unknown), (Level::WARN, "Unknown error"));
        assert_eq!(level_of(unknown, 0), (Level::TRACE, "Unknown error"));
    }

    /// Without the `err_type != 0` guard, a zero type would test as masked and report at `Trace`.
    #[test]
    fn a_zero_error_type_reports_unknown_at_warn() {
        assert_eq!(level_of(0, 0), (Level::WARN, "Unknown error"));
    }

    /// The boundaries are a contract with `[log] level`; LOG_INFO lands below `Info` because php-src reports deprecations there.
    #[test]
    fn syslog_severities_keep_their_boundaries() {
        for (priority, want) in [
            (0, Level::ERROR),
            (1, Level::ERROR),
            (2, Level::ERROR),
            (3, Level::ERROR),
            (4, Level::WARN),
            (5, Level::INFO),
            (6, Level::DEBUG),
            (7, Level::DEBUG),
        ] {
            assert_eq!(syslog_to_level(priority), want, "priority {priority}");
        }
    }

    /// A priority off the scale reports at `Info` rather than being dropped or raised to an error.
    #[test]
    fn an_out_of_range_syslog_priority_falls_back_to_info() {
        assert_eq!(syslog_to_level(8), Level::INFO);
        assert_eq!(syslog_to_level(-1), Level::INFO);
    }

    /// Both tables sort a deprecation below `Info`, so `[log] level = "info"` does not surface deprecations.
    #[test]
    fn both_paths_sort_deprecations_below_info() {
        let (level, label) = level_of(E_DEPRECATED, E_DEPRECATED);
        assert_eq!(label, "Deprecated");
        assert!(level > Level::INFO);
        assert!(syslog_to_level(6) > Level::INFO);
    }
}
