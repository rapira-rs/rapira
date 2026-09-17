use crate::ipc::Sender;
use rapira_config::{LogSettings, OtelSettings};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Config {
    pub otel: OtelSettings,
    pub log: LogSettings,
    pub max_connections: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let (otel, log, max_connections) =
            serde_json::from_str(&std::env::var("RAPIRA_OTEL_CONFIG")?)?;
        Ok(Self {
            otel,
            log,
            max_connections,
        })
    }
}

pub struct Process {
    config: Config,
    sender: Sender,
    listener: UnixListener,
    control_rx: UnixStream,
    control_tx: Option<UnixStream>,
    _directory: tempfile::TempDir,
    pid: Option<libc::pid_t>,
    restart_at: Instant,
    owner: u32,
}

impl Process {
    pub fn prepare(config: Config) -> anyhow::Result<Self> {
        let directory = tempfile::Builder::new().prefix("otel-").tempdir()?;
        let path = directory.path().join("export.sock");
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let (control_rx, control_tx) = UnixStream::pair()?;
        Ok(Self {
            config,
            sender: Sender::new(path),
            listener,
            control_rx,
            control_tx: Some(control_tx),
            _directory: directory,
            pid: None,
            restart_at: Instant::now(),
            owner: std::process::id(),
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub fn parent_fds(&self) -> Vec<RawFd> {
        vec![
            self.listener.as_raw_fd(),
            self.control_rx.as_raw_fd(),
            self.control_tx
                .as_ref()
                .expect("control writer")
                .as_raw_fd(),
        ]
    }

    pub fn tick(&mut self) {
        if self.pid.is_some() || Instant::now() < self.restart_at {
            return;
        }
        match self.spawn() {
            Ok(pid) => {
                self.pid = Some(pid);
                tracing::info!(target: "otel", exporter_pid = pid, "exporter started");
            }
            Err(error) => {
                self.restart_at = Instant::now() + Duration::from_secs(1);
                tracing::error!(target: "otel", %error, "exporter start failed");
            }
        }
    }

    fn spawn(&self) -> anyhow::Result<libc::pid_t> {
        let listener: OwnedFd = self.listener.try_clone()?.into();
        let control: OwnedFd = self.control_rx.try_clone()?.into();
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("otel")
            .env(
                "RAPIRA_OTEL_CONFIG",
                serde_json::to_string(&(
                    &self.config.otel,
                    &self.config.log,
                    self.config.max_connections,
                ))?,
            )
            .stdin(Stdio::from(listener))
            .stdout(Stdio::from(control))
            .process_group(0);
        Ok(command.spawn()?.id() as libc::pid_t)
    }

    pub fn on_exit(&mut self, pid: libc::pid_t, status: libc::c_int) -> bool {
        if self.pid != Some(pid) {
            return false;
        }
        self.pid = None;
        self.restart_at = Instant::now() + Duration::from_secs(1);
        let exit_code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
        let signal = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
        tracing::error!(target: "otel", exporter_pid = pid, exit_code, signal, "exporter exited");
        true
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.owner != std::process::id() {
            return;
        }
        self.sender.disconnect();
        self.control_tx.take();
        let Some(pid) = self.pid.take() else {
            return;
        };
        let deadline =
            Instant::now() + Duration::from_secs(self.config.otel.export_timeout_secs + 1);
        loop {
            let mut status = 0;
            // SAFETY: pid is the exporter child and status is a valid output pointer.
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == pid
                || (result < 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
            {
                break;
            }
            if Instant::now() >= deadline {
                // SAFETY: the unreaped child still owns this PID.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
