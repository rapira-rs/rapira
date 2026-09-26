use anyhow::bail;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

use crate::{ConfigCtx, capped_timeout};

#[derive(Debug)]
pub struct SupervisorSettings {
    pub process_control_timeout: Duration,
    pub pidfile: Option<PathBuf>,
}

impl SupervisorSettings {
    /// The master sends QUIT at t=0 and SIGTERM at `process_control_timeout`, which is SIG_DFL in the child, so the margin is the drain's only room to finish.
    pub fn drain_grace(&self) -> Duration {
        const MARGIN: Duration = Duration::from_secs(5);
        let margin = MARGIN.min(self.process_control_timeout / 2);
        self.process_control_timeout - margin
    }
}

/// The `[supervisor]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSection {
    pidfile: Option<String>,
    process_control_timeout_secs: Option<u64>,
}

pub fn resolve_supervisor(
    section: SupervisorSection,
    ctx: &ConfigCtx,
) -> anyhow::Result<SupervisorSettings> {
    let pidfile = section
        .pidfile
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|p| ctx.resolve_path(p))
        .transpose()?;

    let control_secs = section.process_control_timeout_secs.unwrap_or(30);
    if control_secs == 0 {
        bail!("supervisor.process_control_timeout_secs must be at least 1");
    }

    Ok(SupervisorSettings {
        process_control_timeout: capped_timeout(
            "supervisor",
            "process_control_timeout_secs",
            control_secs,
        )?,
        pidfile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supervisor(toml: &str) -> anyhow::Result<SupervisorSettings> {
        let section: SupervisorSection = toml::from_str(toml)?;
        resolve_supervisor(
            section,
            &ConfigCtx {
                dir: PathBuf::from("/w"),
            },
        )
    }

    #[test]
    fn supervisor_pidfile_resolves_against_config_dir() {
        let s = supervisor("pidfile = \"rapira.pid\"\n").unwrap();
        assert_eq!(s.pidfile, Some(PathBuf::from("/w/rapira.pid")));
    }

    struct Case {
        name: &'static str,
        toml: &'static str,
        error: &'static str,
    }

    /// A zero stop budget escalates at once and leaves the drain no time.
    #[test]
    fn supervisor_errors_name_the_key() {
        let cases = [
            Case {
                name: "zero control timeout",
                toml: "process_control_timeout_secs = 0\n",
                error: "supervisor.process_control_timeout_secs must be at least 1",
            },
            Case {
                name: "control timeout above the cap",
                toml: "process_control_timeout_secs = 100000\n",
                error: "supervisor.process_control_timeout_secs 100000 is too large (max 86400)",
            },
            Case {
                name: "unknown key",
                toml: "bogus = 1\n",
                error: "unknown field `bogus`",
            },
            Case {
                name: "max_requests belongs to the pool",
                toml: "max_requests = 1\n",
                error: "unknown field `max_requests`",
            },
        ];
        for case in cases {
            let err = supervisor(case.toml).expect_err(case.name).to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }

    #[test]
    fn drain_grace_leaves_room_before_the_master_escalates() {
        let grace = |secs| {
            SupervisorSettings {
                process_control_timeout: Duration::from_secs(secs),
                pidfile: None,
            }
            .drain_grace()
        };
        assert_eq!(grace(30), Duration::from_secs(25));
        assert_eq!(grace(60), Duration::from_secs(55));
        assert_eq!(grace(5), Duration::from_millis(2500));
        assert_eq!(grace(1), Duration::from_millis(500));
        for secs in 1..=120 {
            assert!(
                grace(secs) < Duration::from_secs(secs),
                "drain must end before the escalation at {secs}s"
            );
        }
    }
}
