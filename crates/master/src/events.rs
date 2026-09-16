use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::Scoreboard;

use crate::pctl::{Pctl, SignalAction};
use crate::pool::Pool;
use crate::process::{ExitVerdict, ProcTable, Spawner, WorkerProc, reap_all};
use crate::signals::{SIG_CHLD, errno_get};
use crate::{MasterConfig, StopReason};

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Milliseconds until `next`, rounded up so a sub-millisecond remainder never busy-spins `poll` with a 0 timeout.
fn poll_timeout_ms(next: Instant, now: Instant) -> c_int {
    let d = next.saturating_duration_since(now);
    let ms = d.as_nanos().div_ceil(1_000_000);
    ms.min(i32::MAX as u128) as c_int
}

fn drain_pipe(fd: RawFd, buf: &mut [u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        // SAFETY: read into a live buffer from a valid nonblocking fd.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

/// The global half: signals, the stop escalation, and routing to the pools. Every per-pool decision lives in [`Pool`].
pub(crate) struct Master<S: Spawner> {
    pub(crate) service: Box<dyn crate::Service>,
    pools: Vec<Pool>,
    spawner: S,
    pctl: Pctl,
    control_timeout: Duration,
    next_tick: Instant,
    /// Stopping escalation (QUIT, TERM, KILL against every pool).
    stop_deadline: Option<Instant>,
}

impl<S: Spawner> Master<S> {
    /// Lays the pools out over the board: two slots per worker, contiguous, in `cfg.pools` order.
    pub(crate) fn new(cfg: MasterConfig, scoreboard: Scoreboard, spawner: S) -> Master<S> {
        let now = Instant::now();
        let control_timeout = cfg.process_control_timeout;
        let mut base: usize = 0;
        let pools: Vec<Pool> = cfg
            .pools
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let len = p.slots();
                let board = scoreboard.slice(base..base + len);
                base += len;
                Pool::new(i, p, board, control_timeout)
            })
            .collect();
        Master {
            service: Box::new(()),
            pools,
            spawner,
            pctl: Pctl::default(),
            control_timeout,
            next_tick: now + Duration::from_secs(1),
            stop_deadline: None,
        }
    }

    /// `Some(reason)` means the loop must return now: forced stop, or a stop with nothing left to drain.
    fn handle_signal(&mut self, byte: u8, now: Instant) -> Option<StopReason> {
        match self.pctl.on_signal(byte) {
            SignalAction::Stop => self.begin_stop(now),
            SignalAction::Forced => {
                self.force_stop();
                Some(StopReason::Forced)
            }
            SignalAction::Reload => {
                // A pool that still drains its chain swallows the signal.
                if self.pools.iter().all(|p| p.reload.is_none()) {
                    self.begin_reload(now);
                }
                None
            }
            SignalAction::Status => {
                self.log_status();
                None
            }
            SignalAction::Ignore => None,
        }
    }

    fn begin_stop(&mut self, now: Instant) -> Option<StopReason> {
        for p in &mut self.pools {
            p.begin_stop();
        }
        self.stop_deadline = Some(now + self.control_timeout);
        if self.drained() {
            return Some(StopReason::Drained);
        }
        None
    }

    fn force_stop(&self) {
        for p in &self.pools {
            p.signal_all(libc::SIGTERM);
        }
    }

    fn escalate_stop(&mut self, now: Instant) {
        if let Some(sig) = self.pctl.escalate() {
            for p in &self.pools {
                p.signal_all(sig);
            }
            self.stop_deadline = Some(now + Duration::from_secs(1));
        }
    }

    fn drained(&self) -> bool {
        self.pools.iter().all(|p| p.table.procs.is_empty())
    }

    fn begin_reload(&mut self, now: Instant) {
        for p in &mut self.pools {
            p.begin_reload(now, &mut self.spawner);
        }
    }

    fn route_exit(
        &mut self,
        pool: usize,
        w: WorkerProc,
        verdict: ExitVerdict,
        now: Instant,
    ) -> anyhow::Result<()> {
        let stopping = self.pctl.is_stopping();
        self.pools[pool].on_child_exit(w, verdict, now, stopping, &mut self.spawner)?;
        Ok(())
    }

    fn reap(&mut self, now: Instant) -> anyhow::Result<()> {
        let buried = {
            let mut tables: Vec<&mut ProcTable> =
                self.pools.iter_mut().map(|p| &mut p.table).collect();
            reap_all(&mut tables, &mut *self.service)
        };
        for (pool, w, verdict, status) in buried {
            let exit_code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
            let signal = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
            if !self.pctl.is_stopping()
                && matches!(
                    verdict,
                    ExitVerdict::Crash | ExitVerdict::TimeoutKill | ExitVerdict::Unhealthy
                )
            {
                tracing::error!(target: "master", pool = self.pools[pool].cfg.name,
                    worker_pid = w.pid, exit_code, signal, reason = ?verdict, "worker failed");
            } else {
                tracing::info!(target: "master", pool = self.pools[pool].cfg.name,
                    worker_pid = w.pid, exit_code, signal, reason = ?verdict, "worker exited");
            }
            self.route_exit(pool, w, verdict, now)?;
        }
        Ok(())
    }

    fn fire_due_deadlines(&mut self, now: Instant) {
        if let Some(t) = self.stop_deadline
            && now >= t
        {
            self.escalate_stop(now);
        }
        for p in &mut self.pools {
            p.fire_due(now, &mut self.spawner);
        }
        if now >= self.next_tick {
            self.next_tick = now + Duration::from_secs(1);
            self.service.tick();
            let stopping = self.pctl.is_stopping();
            for p in &mut self.pools {
                p.maintenance_tick(now, stopping, &mut self.spawner);
            }
        }
    }

    fn next_deadline(&self) -> Instant {
        let mut next = self.next_tick;
        for d in self
            .pools
            .iter()
            .filter_map(|p| p.next_deadline())
            .chain(self.stop_deadline)
        {
            next = next.min(d);
        }
        next
    }

    /// `fds[0]` is the signal fd; `owners[k]` is the pool that owns `fds[k + 1]`.
    fn poll_set(&self) -> (Vec<libc::pollfd>, Vec<usize>) {
        let stopping = self.pctl.is_stopping();
        let mut fds: Vec<libc::pollfd> = vec![pollfd(self.spawner.signal_fd())];
        let mut owners: Vec<usize> = Vec::new();
        for (i, p) in self.pools.iter().enumerate() {
            if !p.armed(stopping) {
                continue;
            }
            for &fd in &p.cfg.listeners {
                fds.push(pollfd(fd));
                owners.push(i);
            }
        }
        (fds, owners)
    }

    /// Re-checks arming: this iteration's signals and reaps can disarm a pool after `poll` returned its listener readable.
    fn fork_readable(&mut self, readable: &[usize], now: Instant) {
        let stopping = self.pctl.is_stopping();
        let mut last: Option<usize> = None;
        for &i in readable {
            if last == Some(i) {
                continue;
            }
            last = Some(i);
            if self.pools[i].armed(stopping) {
                self.pools[i].ondemand_fork_one(now, &mut self.spawner);
            }
        }
    }

    fn log_status(&self) {
        for p in &self.pools {
            p.log_status();
        }
    }

    pub(crate) fn run_loop(&mut self) -> anyhow::Result<StopReason> {
        self.service.tick();
        let start = Instant::now();
        for p in &mut self.pools {
            p.fork_initial(start, &mut self.spawner);
        }
        loop {
            let (mut fds, owners) = self.poll_set();

            let timeout = poll_timeout_ms(self.next_deadline(), Instant::now());
            // SAFETY: fds is a live slice; timeout is a valid millisecond count.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            if n < 0 {
                if errno_get() == libc::EINTR {
                    continue;
                }
                anyhow::bail!("poll: {}", std::io::Error::last_os_error());
            }
            let now = Instant::now();

            let mut got_chld: bool = false;
            if n > 0 && (fds[0].revents & libc::POLLIN) != 0 {
                let mut buf = [0u8; 64];
                for b in drain_pipe(self.spawner.signal_fd(), &mut buf) {
                    if b == SIG_CHLD {
                        got_chld = true;
                    } else if let Some(reason) = self.handle_signal(b, now) {
                        return Ok(reason);
                    }
                }
            }

            if got_chld {
                self.reap(now)?;
                if self.pctl.is_stopping() && self.drained() {
                    return Ok(StopReason::Drained);
                }
            }

            if n > 0 && !owners.is_empty() {
                let readable: Vec<usize> = owners
                    .iter()
                    .enumerate()
                    .filter(|&(k, _)| (fds[k + 1].revents & libc::POLLIN) != 0)
                    .map(|(_, &i)| i)
                    .collect();
                self.fork_readable(&readable, now);
            }

            self.fire_due_deadlines(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{Reload, ReloadPhase, WarnCounter};
    use crate::process::{FakeSpawner, KillIntent, TestChild, dead_worker, wait_signal};
    use crate::signals::{SIG_TERM, SIG_USR2};
    use crate::{PoolConfig, Scaling, StopReason};
    use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, now_millis};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::sync::atomic::Ordering::{Relaxed, Release};
    use std::sync::{Arc, Mutex};

    // Sentinel pids above PID_MAX_LIMIT: the QUIT/TERM/KILL these tests send resolve to ESRCH.
    const P_A: libc::pid_t = 2_000_000_001;
    const P_B: libc::pid_t = 2_000_000_002;

    // Indexed by spec position; a third spec panics.
    const POOL_NAMES: [&str; 2] = ["http", "grpc"];

    /// Two pools: "http" of `specs[0]` and "grpc" of `specs[1]`. The returned board is the whole mapping the pools slice.
    fn test_master(specs: &[(usize, Scaling)]) -> (Master<FakeSpawner>, Scoreboard) {
        let slots: usize = specs.iter().map(|(n, _)| n * 2).sum();
        let sb: Scoreboard = Scoreboard::create(slots).unwrap();
        let pools: Vec<PoolConfig> = specs
            .iter()
            .enumerate()
            .map(|(i, &(processes, scaling))| PoolConfig {
                name: POOL_NAMES[i],
                processes,
                scaling,
                process_idle_timeout: Duration::from_secs(10),
                request_terminate_timeout: Duration::ZERO,
                listeners: Vec::new(),
            })
            .collect();
        let cfg: MasterConfig = MasterConfig {
            pools,
            process_control_timeout: Duration::from_secs(30),
            pidfile: None,
        };
        (Master::new(cfg, sb, FakeSpawner::new()), sb)
    }

    /// Worker generation of the http pool and the grpc pool.
    fn generations(m: &Master<FakeSpawner>) -> (u32, u32) {
        (m.pools[0].table.generation, m.pools[1].table.generation)
    }

    /// A live fd for the poll set. These tests read the owner map, never the fd.
    fn listener_fd(keep: &mut Vec<OwnedFd>) -> RawFd {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair fills a 2-element array with two owned fds.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair");
        for fd in fds {
            // SAFETY: fds holds two fresh fds we take sole ownership of.
            keep.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        fds[0]
    }

    /// Captures `master` INFO messages; hand-rolled because the crate depends only on the `tracing` facade.
    #[derive(Clone, Default)]
    struct MsgCapture(Arc<Mutex<Vec<String>>>);

    impl MsgCapture {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl tracing::field::Visit for MsgCapture {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.lock().unwrap().push(format!("{value:?}"));
            }
        }
    }

    impl tracing::Subscriber for MsgCapture {
        fn enabled(&self, md: &tracing::Metadata) -> bool {
            md.target() == "master" && *md.level() == tracing::Level::INFO
        }
        fn event(&self, ev: &tracing::Event) {
            ev.record(&mut self.clone());
        }
        fn new_span(&self, _: &tracing::span::Attributes) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn poll_timeout_rounds_up_never_to_zero_before_deadline() {
        let base = Instant::now();
        assert_eq!(poll_timeout_ms(base + Duration::from_micros(1500), base), 2);
        assert_eq!(poll_timeout_ms(base + Duration::from_nanos(1), base), 1);
        assert_eq!(poll_timeout_ms(base + Duration::from_millis(3), base), 3);
        assert_eq!(poll_timeout_ms(base, base + Duration::from_millis(5)), 0);
    }

    #[test]
    fn pools_get_disjoint_contiguous_views() {
        let (m, sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        assert_eq!(m.pools[0].board.nslots(), 4);
        assert_eq!(m.pools[1].board.nslots(), 2);
        assert!(std::ptr::eq(m.pools[0].board.slot(0), sb.slot(0)));
        assert!(std::ptr::eq(m.pools[1].board.slot(0), sb.slot(4)));
        assert_eq!((m.pools[0].index, m.pools[1].index), (0, 1));
    }

    #[test]
    fn fork_initial_spawns_each_pool_inside_its_own_range() {
        let (mut m, sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        for p in &mut m.pools {
            p.fork_initial(t0, &mut m.spawner);
        }

        let calls = &m.spawner.calls;
        assert_eq!(calls.len(), 3);
        assert_eq!(calls.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0, 0, 1]);
        assert!(std::ptr::eq(calls[0].1, sb.slot(0)));
        assert!(std::ptr::eq(calls[1].1, sb.slot(1)));
        assert!(std::ptr::eq(calls[2].1, sb.slot(4)));
    }

    #[test]
    fn second_reload_is_ignored_until_every_pool_finished() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        for (i, &pid) in [P_A, P_B].iter().enumerate() {
            m.pools[i].push_proc(pid, 0, 0, t0);
            m.pools[i].set_slot(0, SLOT_IDLE);
        }

        assert_eq!(m.handle_signal(SIG_USR2, t0), None);
        assert!(m.pools.iter().all(|p| p.reload.is_some()));
        assert_eq!(generations(&m), (1, 1));

        for p in &mut m.pools {
            p.set_slot(1, SLOT_IDLE);
        }
        m.fire_due_deadlines(t0 + Duration::from_millis(60));

        assert_eq!(m.handle_signal(SIG_USR2, t0), None);
        assert_eq!(generations(&m), (1, 1), "both pools still drain");

        let w = m.pools[0].take_proc(P_A);
        m.route_exit(0, w, ExitVerdict::Drain, t0).unwrap();
        assert!(m.pools[0].reload.is_none());

        assert_eq!(m.handle_signal(SIG_USR2, t0), None);
        assert_eq!(generations(&m), (1, 1), "the grpc pool still drains");

        let w = m.pools[1].take_proc(P_B);
        m.route_exit(1, w, ExitVerdict::Drain, t0).unwrap();
        assert!(m.pools.iter().all(|p| p.reload.is_none()));

        assert_eq!(m.handle_signal(SIG_USR2, t0), None);
        assert_eq!(generations(&m), (2, 2));
    }

    #[test]
    fn reload_skips_pools_without_old_workers() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        m.pools[0].push_proc(P_A, 0, 0, t0);
        m.pools[0].set_slot(0, SLOT_IDLE);

        m.handle_signal(SIG_USR2, t0);

        assert!(m.pools[0].reload.is_some());
        assert!(m.pools[1].reload.is_none(), "no old worker to drain");
        assert_eq!(m.pools[1].table.generation, 1, "the generation still moves");
        assert_eq!(m.spawner.calls.len(), 1);
        assert_eq!(m.spawner.calls[0].0, 0);
    }

    #[test]
    fn maintenance_continues_in_pools_that_are_not_reloading() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now() + Duration::from_secs(2);
        m.pools[0].reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 0,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0 + Duration::from_secs(30),
        });

        m.fire_due_deadlines(t0);

        assert_eq!(
            m.spawner.calls.iter().map(|c| c.0).collect::<Vec<_>>(),
            vec![1],
            "only the pool that is not reloading refills"
        );
    }

    #[test]
    fn stop_quits_every_pool_and_clears_pending_reloads() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        for (i, &pid) in [P_A, P_B].iter().enumerate() {
            m.pools[i].push_proc(pid, 0, 0, t0);
            m.pools[i].set_slot(0, SLOT_IDLE);
            m.pools[i].table.slots[1].schedule_immediate(t0);
        }
        m.handle_signal(SIG_USR2, t0);
        assert!(m.pools.iter().all(|p| p.reload.is_some()));

        assert_eq!(m.handle_signal(SIG_TERM, t0), None);

        assert!(m.pctl.is_stopping());
        assert_eq!(m.stop_deadline, Some(t0 + Duration::from_secs(30)));
        for p in &m.pools {
            assert!(p.reload.is_none(), "{} kept a reload chain", p.cfg.name);
            assert!(p.table.slots.iter().all(|s| s.respawn_at.is_none()));
        }
    }

    #[test]
    fn begin_stop_with_no_workers_is_drained_immediately() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        assert_eq!(
            m.handle_signal(SIG_TERM, Instant::now()),
            Some(StopReason::Drained)
        );
    }

    #[test]
    fn escalation_under_stop_hits_every_pool() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        let mut children: Vec<TestChild> = Vec::new();
        for i in 0..2 {
            let child = TestChild::sleeper();
            m.pools[i].push_proc(child.pid(), 0, 0, t0);
            children.push(child);
        }

        m.pctl.on_signal(SIG_TERM);
        m.escalate_stop(t0);

        assert_eq!(m.stop_deadline, Some(t0 + Duration::from_secs(1)));
        for child in &mut children {
            assert_eq!(wait_signal(child), Some(libc::SIGTERM));
        }
    }

    #[test]
    fn gen0_unhealthy_in_pool_b_failboots_the_master_while_pool_a_serves() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        m.pools[0].push_proc(P_A, 0, 0, t0);
        m.pools[0].set_slot(0, SLOT_ACTIVE);
        m.pools[0].board.slot(0).handled.store(7, Release);

        let w = dead_worker(P_B, 0, 0, t0);
        let e = m
            .route_exit(1, w, ExitVerdict::Unhealthy, t0)
            .unwrap_err()
            .to_string();

        assert!(e.contains("grpc pool: worker"), "{e}");
        assert_eq!(m.pools[0].table.running(), 1, "pool a keeps its worker");
    }

    #[test]
    fn served_history_is_per_pool() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        m.pools[0].board.slot(0).handled.store(1, Release);

        let w = dead_worker(P_A, 0, 0, t0);
        m.route_exit(0, w, ExitVerdict::Unhealthy, t0)
            .expect("the http pool served a request");

        let w = dead_worker(P_B, 0, 0, t0);
        assert!(
            m.route_exit(1, w, ExitVerdict::Unhealthy, t0).is_err(),
            "the grpc pool never served, so it failboots"
        );
    }

    #[test]
    fn ondemand_listeners_are_polled_per_armed_pool() {
        let (mut m, _sb) = test_master(&[(1, Scaling::Ondemand), (1, Scaling::Ondemand)]);
        let mut keep: Vec<OwnedFd> = Vec::new();
        for p in &mut m.pools {
            p.cfg.listeners = vec![listener_fd(&mut keep)];
        }
        m.pools[1].push_proc(P_B, 0, 0, Instant::now());
        m.pools[1].set_slot(0, SLOT_IDLE);

        let (fds, owners) = m.poll_set();
        assert_eq!(owners, vec![0], "an idle worker disarms its own pool");
        assert_eq!(fds.len(), 2);
        assert_eq!(fds[0].fd, m.spawner.signal_fd());
        assert_eq!(fds[1].fd, m.pools[0].cfg.listeners[0]);

        m.pools[1].take_proc(P_B);
        m.pools[1].set_slot(0, SLOT_FREE);
        assert_eq!(m.poll_set().1, vec![0, 1]);

        m.pctl.on_signal(SIG_TERM);
        assert!(m.poll_set().1.is_empty(), "a stopping master polls nothing");
    }

    #[test]
    fn ondemand_fork_skips_a_pool_that_lost_arming_after_the_poll() {
        let (mut m, _sb) = test_master(&[(1, Scaling::Ondemand), (1, Scaling::Ondemand)]);
        let t0 = Instant::now();
        let mut keep: Vec<OwnedFd> = Vec::new();
        for p in &mut m.pools {
            p.cfg.listeners = vec![listener_fd(&mut keep)];
        }
        assert_eq!(m.poll_set().1, vec![0, 1], "both pools poll their listener");

        m.pctl.on_signal(SIG_TERM);
        m.fork_readable(&[0, 1], t0);
        assert!(
            m.spawner.calls.is_empty(),
            "a readable listener must not outlive the stop"
        );

        m.pctl = Pctl::default();
        m.fork_readable(&[0, 0, 1], t0);
        assert_eq!(
            m.spawner.calls.iter().map(|c| c.0).collect::<Vec<_>>(),
            vec![0, 1],
            "one fork per readable pool"
        );
    }

    #[test]
    fn watchdog_timeout_is_per_pool() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now() + Duration::from_secs(2);
        m.pools[0].cfg.request_terminate_timeout = Duration::from_secs(2);
        for (i, &pid) in [P_A, P_B].iter().enumerate() {
            m.pools[i].push_proc(pid, 0, 0, t0);
            m.pools[i].set_slot(0, SLOT_ACTIVE);
            m.pools[i]
                .board
                .slot(0)
                .last_activity_ms
                .store(now_millis().saturating_sub(5_000), Relaxed);
        }

        m.fire_due_deadlines(t0);

        assert_eq!(
            m.pools[0].table.procs[0].kill_intent,
            Some(KillIntent::Timeout)
        );
        assert_eq!(
            m.pools[1].table.procs[0].kill_intent, None,
            "a zero timeout keeps the grpc worker"
        );
    }

    #[test]
    fn dynamic_ceiling_warning_is_per_pool() {
        let dynamic = Scaling::Dynamic {
            min_spare: 2,
            max_spare: 4,
        };
        let (mut m, _sb) = test_master(&[(2, dynamic), (2, dynamic)]);
        let t0 = Instant::now() + Duration::from_secs(2);
        for p in &mut m.pools {
            for (i, &pid) in [P_A, P_B].iter().enumerate() {
                p.push_proc(pid, i, 0, t0);
                p.set_slot(i, SLOT_ACTIVE);
            }
        }

        let warns = WarnCounter::default();
        tracing::subscriber::with_default(warns.clone(), || {
            m.fire_due_deadlines(t0);
            assert_eq!(warns.count(), 2, "one warning per pool at its ceiling");
            m.fire_due_deadlines(t0 + Duration::from_secs(1));
        });
        assert_eq!(warns.count(), 2, "each pool warns once");
    }

    #[test]
    fn status_logs_a_block_per_pool() {
        let (m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        m.pools[1].board.slot(0).bind(4242);

        let msgs = MsgCapture::default();
        tracing::subscriber::with_default(msgs.clone(), || m.log_status());

        let lines = msgs.lines();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(
            lines[0],
            "status: http pool: 0 running, 0 idle, generation 0"
        );
        assert_eq!(
            lines[1],
            "status: grpc pool: 0 running, 1 idle, generation 0"
        );
        assert!(lines[2].starts_with("  slot 0 pid 4242"), "{}", lines[2]);
    }

    #[test]
    fn next_deadline_is_the_earliest_across_pools() {
        let (mut m, _sb) = test_master(&[(2, Scaling::Static), (1, Scaling::Static)]);
        let t0 = Instant::now();
        m.next_tick = t0 + Duration::from_secs(1);
        assert_eq!(m.next_deadline(), t0 + Duration::from_secs(1));

        m.pools[0].table.slots[0].respawn_at = Some(t0 + Duration::from_millis(700));
        m.pools[1].reload = Some(Reload {
            phase: ReloadPhase::Await {
                slot: 0,
                until: t0 + Duration::from_secs(30),
            },
            deadline: t0 + Duration::from_millis(200),
        });
        assert_eq!(m.next_deadline(), t0 + Duration::from_millis(200));

        m.stop_deadline = Some(t0 + Duration::from_millis(100));
        assert_eq!(m.next_deadline(), t0 + Duration::from_millis(100));
    }
}
