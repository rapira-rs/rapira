//! The stop state of the master: the signal precedence and the TERM then KILL escalation after the QUIT.

use std::time::{Duration, Instant};

use libc::c_int;

/// Master-wide control state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Pctl {
    #[default]
    Normal,
    Stopping {
        /// The signal the escalation sends at `deadline`: TERM once, then KILL.
        next: c_int,
        deadline: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalAction {
    Stop,
    Forced,
    Reload,
    Status,
    Ignore,
}

impl Pctl {
    pub fn is_stopping(&self) -> bool {
        matches!(self, Pctl::Stopping { .. })
    }

    /// TERM, INT or QUIT in Normal starts a graceful stop and arms the first escalation at `stop_deadline`. While stopping, TERM or INT forces the stop and QUIT is ignored.
    pub fn on_signal(&mut self, signo: c_int, stop_deadline: Instant) -> SignalAction {
        match signo {
            libc::SIGTERM | libc::SIGINT | libc::SIGQUIT => match *self {
                Pctl::Stopping { .. } if signo == libc::SIGQUIT => SignalAction::Ignore,
                Pctl::Stopping { .. } => SignalAction::Forced,
                Pctl::Normal => {
                    *self = Pctl::Stopping {
                        next: libc::SIGTERM,
                        deadline: stop_deadline,
                    };
                    SignalAction::Stop
                }
            },
            libc::SIGUSR2 | libc::SIGHUP => match *self {
                Pctl::Normal => SignalAction::Reload,
                Pctl::Stopping { .. } => SignalAction::Ignore,
            },
            libc::SIGUSR1 => SignalAction::Status,
            _ => SignalAction::Ignore,
        }
    }

    pub fn stop_deadline(&self) -> Option<Instant> {
        match self {
            Pctl::Normal => None,
            Pctl::Stopping { deadline, .. } => Some(*deadline),
        }
    }

    /// Runs only at the stop deadline: sets the next deadline one second after `now` and returns the signal for every worker of every pool.
    pub fn escalate(&mut self, now: Instant) -> c_int {
        let Pctl::Stopping { next, deadline } = self else {
            unreachable!("stop escalation outside a stop");
        };
        *deadline = now + Duration::from_secs(1);
        std::mem::replace(next, libc::SIGKILL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_signal_in_normal_enters_stopping() {
        struct Case {
            name: &'static str,
            signo: c_int,
        }
        let cases = [
            Case {
                name: "TERM",
                signo: libc::SIGTERM,
            },
            Case {
                name: "INT",
                signo: libc::SIGINT,
            },
            Case {
                name: "QUIT",
                signo: libc::SIGQUIT,
            },
        ];
        let t0 = Instant::now();
        for case in cases {
            let mut p = Pctl::default();
            assert_eq!(
                p.on_signal(case.signo, t0),
                SignalAction::Stop,
                "{}",
                case.name
            );
            assert_eq!(
                p,
                Pctl::Stopping {
                    next: libc::SIGTERM,
                    deadline: t0
                },
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn second_stop_signal_is_forced() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(libc::SIGTERM, t0), SignalAction::Stop);
        assert_eq!(p.on_signal(libc::SIGTERM, t0), SignalAction::Forced);
        assert_eq!(p.on_signal(libc::SIGINT, t0), SignalAction::Forced);
    }

    #[test]
    fn retried_quit_stays_graceful() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(libc::SIGQUIT, t0), SignalAction::Stop);
        assert_eq!(p.on_signal(libc::SIGQUIT, t0), SignalAction::Ignore);
        assert!(p.is_stopping());
        assert_eq!(p.on_signal(libc::SIGTERM, t0), SignalAction::Forced);
    }

    #[test]
    fn a_reload_signal_in_normal_reloads() {
        struct Case {
            name: &'static str,
            signo: c_int,
        }
        let cases = [
            Case {
                name: "USR2",
                signo: libc::SIGUSR2,
            },
            Case {
                name: "HUP",
                signo: libc::SIGHUP,
            },
        ];
        let t0 = Instant::now();
        for case in cases {
            let mut p = Pctl::default();
            assert_eq!(
                p.on_signal(case.signo, t0),
                SignalAction::Reload,
                "{}",
                case.name
            );
            assert_eq!(p, Pctl::Normal, "{}", case.name);
        }
    }

    #[test]
    fn reload_ignored_while_stopping() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        p.on_signal(libc::SIGTERM, t0);
        assert_eq!(p.on_signal(libc::SIGUSR2, t0), SignalAction::Ignore);
        assert!(p.is_stopping());
    }

    #[test]
    fn status_is_stateless() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(libc::SIGUSR1, t0), SignalAction::Status);
        assert_eq!(p, Pctl::Normal);
        p.on_signal(libc::SIGTERM, t0);
        assert_eq!(p.on_signal(libc::SIGUSR1, t0), SignalAction::Status);
        assert!(p.is_stopping());
    }

    #[test]
    fn stop_escalation_sends_term_then_kill() {
        let t0 = Instant::now();
        let mut p = Pctl::default();
        p.on_signal(libc::SIGTERM, t0);
        assert_eq!(p.escalate(t0), libc::SIGTERM);
        assert_eq!(p.stop_deadline(), Some(t0 + Duration::from_secs(1)));
        assert_eq!(p.escalate(t0), libc::SIGKILL);
        assert_eq!(p.escalate(t0), libc::SIGKILL);
    }
}
