use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{error, warn};

use crate::pool::{Pool, now_millis};

const TICK: Duration = Duration::from_secs(1);
const GRACE: Duration = Duration::from_secs(5);

#[repr(C)]
struct Interrupt {
    timed_out: *mut c_void,
    vm_interrupt: *mut c_void,
    thread: libc::pthread_t,
}

// SAFETY: C accesses only the two atomic flags. The registry lock keeps their PHP thread alive during a tick.
unsafe impl Send for Interrupt {}

unsafe extern "C" {
    fn rapira_watchdog_install() -> c_int;
    fn rapira_watchdog_block_timer() -> c_int;
    fn rapira_watchdog_capture() -> Interrupt;
    fn rapira_watchdog_fire(slot: &Interrupt);
    fn rapira_watchdog_clear(slot: &Interrupt);
}

fn signal_result(code: c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// Only PHP workers may receive the execution timer signal.
pub fn block_timer_signal() -> io::Result<()> {
    // SAFETY: blocks the static timer signal on the calling thread via pthread_sigmask.
    signal_result(unsafe { rapira_watchdog_block_timer() })
}

/// Call on the main thread after PHP module startup and before any pool starts.
pub(crate) fn reserve_wake_signal() -> io::Result<()> {
    // SAFETY: installs the process-wide async-signal-safe wake handler before worker threads spawn.
    signal_result(unsafe { rapira_watchdog_install() })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escalation {
    None,
    Interrupt,
    Blocked { all_blocked: bool },
}

enum Unit {
    Idle,
    Active(u64),
    Interrupted(Instant),
    Blocked,
}

impl Unit {
    fn interrupted(&self) -> bool {
        matches!(self, Self::Interrupted(_) | Self::Blocked)
    }

    /// Replaces the unit state, updating `blocked` and returning true when an active interrupt must be cleared.
    fn replace(&mut self, next: Self, blocked: &AtomicUsize) -> bool {
        if matches!(self, Self::Blocked) {
            blocked.fetch_sub(1, Relaxed);
        }
        let clear_interrupt = self.interrupted();
        *self = next;
        clear_interrupt
    }

    /// Evaluates deadline escalation at `(now_ms, now)` without side effects on signals or process exit.
    fn check_deadline(
        &mut self,
        now_ms: u64,
        now: Instant,
        timeout: Duration,
        blocked: &AtomicUsize,
        total_workers: usize,
    ) -> Escalation {
        match *self {
            Self::Active(since)
                if Duration::from_millis(now_ms.saturating_sub(since)) >= timeout =>
            {
                *self = Self::Interrupted(now);
                Escalation::Interrupt
            }
            Self::Interrupted(at) if now.saturating_duration_since(at) >= GRACE => {
                *self = Self::Blocked;
                let all_blocked = blocked.fetch_add(1, Relaxed) + 1 == total_workers;
                Escalation::Blocked { all_blocked }
            }
            _ => Escalation::None,
        }
    }
}

struct Worker {
    pool: Arc<Pool>,
    index: usize,
    timeout: Duration,
    interrupt: Interrupt,
    wake: watch::Sender<bool>,
    unit: Unit,
}

type Slot = Arc<Mutex<Worker>>;
static WORKERS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());
thread_local! {
    static LOCAL: RefCell<Option<Slot>> = const { RefCell::new(None) };
}

/// The guard must stay on the PHP thread that owns the flags. It keeps the signal mask that the thread inherited.
#[must_use]
pub(crate) struct Guard {
    inherited: libc::sigset_t,
    thread: PhantomData<Rc<()>>,
}

/// # Safety
/// Call once after this worker's `rapira_thread_start`. Drop the guard before `rapira_thread_end`.
pub(crate) unsafe fn enter(pool: Arc<Pool>, index: usize, timeout: Duration) -> Guard {
    // A child process inherits the signal mask of the thread that starts it, so a PHP thread blocks no signal. https://man7.org/linux/man-pages/man7/signal.7.html
    // SAFETY: sigemptyset initializes the new mask, and pthread_sigmask writes the old mask.
    let inherited = unsafe {
        let mut none = std::mem::zeroed();
        let mut inherited = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        signal_result(libc::pthread_sigmask(
            libc::SIG_SETMASK,
            &none,
            &mut inherited,
        ))
        .expect("unblock the PHP thread signals");
        inherited
    };
    if !timeout.is_zero() {
        let slot = Arc::new(Mutex::new(Worker {
            pool,
            index,
            timeout,
            // SAFETY: called on this PHP thread after `rapira_thread_start`, before `Guard` drops.
            interrupt: unsafe { rapira_watchdog_capture() },
            wake: watch::Sender::new(false),
            unit: Unit::Idle,
        }));
        WORKERS.lock().unwrap().push(slot.clone());
        LOCAL.set(Some(slot));
    }
    Guard {
        inherited,
        thread: PhantomData,
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        clear();
        if let Some(slot) = LOCAL.take() {
            // A tick keeps this lock until its last access to the flags and the pthread ID.
            WORKERS.lock().unwrap().retain(|s| !Arc::ptr_eq(s, &slot));
        }
        // SAFETY: pthread_sigmask copies the mask that it wrote in enter.
        signal_result(unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.inherited, std::ptr::null_mut())
        })
        .expect("block the PHP thread signals before shutdown");
    }
}

pub(crate) fn arm(last_activity_ms: u64) {
    set_unit(Unit::Active(last_activity_ms));
}

pub(crate) fn clear() {
    set_unit(Unit::Idle);
}

pub(crate) fn wake() -> Option<watch::Receiver<bool>> {
    LOCAL.with_borrow(|slot| Some(slot.as_ref()?.lock().unwrap().wake.subscribe()))
}

fn set_unit(unit: Unit) {
    LOCAL.with_borrow(|slot| {
        if let Some(slot) = slot {
            let mut guard = slot.lock().unwrap();
            let worker = &mut *guard;
            if worker.unit.replace(unit, &worker.pool.blocked) {
                // The previous fire has returned. Withdraw its timeout before the next unit or boot.
                // SAFETY: `worker.interrupt` belongs to this live PHP thread and is protected by `slot`.
                unsafe { rapira_watchdog_clear(&worker.interrupt) };
                worker.wake.send_replace(false);
            }
        }
    });
}

impl Worker {
    fn tick(&mut self) {
        let (pool, index, timeout) = (self.pool.name, self.index, self.timeout);
        match self.unit.check_deadline(
            now_millis(),
            Instant::now(),
            timeout,
            &self.pool.blocked,
            self.pool.workers.len(),
        ) {
            Escalation::None => {}
            Escalation::Interrupt => {
                // SAFETY: `self` is locked under `WORKERS`, so the target worker thread has not exited `Guard::drop`.
                unsafe { rapira_watchdog_fire(&self.interrupt) };
                self.wake.send_replace(true);
                warn!(target: "rapira", "{pool}.pool.request_terminate_timeout_secs exceeded on worker {index} ({timeout:?}); interrupting the request");
            }
            Escalation::Blocked { all_blocked } => {
                error!(target: "rapira", "{pool} worker {index} is blocked {GRACE:?} after its interrupt; the unit will end when its native call returns");
                if all_blocked {
                    error!(target: "rapira", "every worker of pool {pool} is blocked; exiting with code 124");
                    crate::exit::exit(124);
                }
            }
        }
    }
}

pub(crate) struct Watchdog {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

/// Start after worker startup when at least one pool has a request timeout.
pub(crate) fn start() -> io::Result<Watchdog> {
    let (stop, rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("rapira-watchdog".into())
        .spawn(move || {
            while rx.recv_timeout(TICK) == Err(RecvTimeoutError::Timeout) {
                for slot in &*WORKERS.lock().unwrap() {
                    let mut worker = slot.lock().unwrap();
                    if rx.try_recv().is_ok() {
                        return;
                    }
                    worker.tick();
                }
            }
        })?;
    Ok(Watchdog {
        stop,
        thread: Some(thread),
    })
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!(target: "rapira", "watchdog thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_unit_escalation_and_recovery() {
        enum Start {
            Idle,
            Active(u64),
            InterruptedAgo(Duration),
            Blocked,
        }
        struct Case {
            name: &'static str,
            start: Start,
            now_ms: u64,
            timeout: Duration,
            initial_blocked: usize,
            total_workers: usize,
            want_escalation: Escalation,
            want_blocked_after_tick: usize,
            want_clear_on_idle: bool,
            want_blocked_after_idle: usize,
        }
        let cases = [
            Case {
                name: "idle worker never escalates",
                start: Start::Idle,
                now_ms: 10_000,
                timeout: Duration::from_secs(2),
                initial_blocked: 0,
                total_workers: 2,
                want_escalation: Escalation::None,
                want_blocked_after_tick: 0,
                want_clear_on_idle: false,
                want_blocked_after_idle: 0,
            },
            Case {
                name: "active worker below timeout stays active",
                start: Start::Active(1_000),
                now_ms: 2_999,
                timeout: Duration::from_secs(2),
                initial_blocked: 0,
                total_workers: 2,
                want_escalation: Escalation::None,
                want_blocked_after_tick: 0,
                want_clear_on_idle: false,
                want_blocked_after_idle: 0,
            },
            Case {
                name: "active worker at timeout interrupts and requires clear on recovery",
                start: Start::Active(1_000),
                now_ms: 3_000,
                timeout: Duration::from_secs(2),
                initial_blocked: 0,
                total_workers: 2,
                want_escalation: Escalation::Interrupt,
                want_blocked_after_tick: 0,
                want_clear_on_idle: true,
                want_blocked_after_idle: 0,
            },
            Case {
                name: "interrupted worker before grace period stays interrupted",
                start: Start::InterruptedAgo(GRACE - Duration::from_millis(1)),
                now_ms: 10_000,
                timeout: Duration::from_secs(2),
                initial_blocked: 0,
                total_workers: 2,
                want_escalation: Escalation::None,
                want_blocked_after_tick: 0,
                want_clear_on_idle: true,
                want_blocked_after_idle: 0,
            },
            Case {
                name: "interrupted worker past grace marks blocked without all_blocked when peers remain",
                start: Start::InterruptedAgo(GRACE),
                now_ms: 10_000,
                timeout: Duration::from_secs(2),
                initial_blocked: 0,
                total_workers: 2,
                want_escalation: Escalation::Blocked { all_blocked: false },
                want_blocked_after_tick: 1,
                want_clear_on_idle: true,
                want_blocked_after_idle: 0,
            },
            Case {
                name: "last interrupted worker past grace triggers all_blocked",
                start: Start::InterruptedAgo(GRACE),
                now_ms: 10_000,
                timeout: Duration::from_secs(2),
                initial_blocked: 1,
                total_workers: 2,
                want_escalation: Escalation::Blocked { all_blocked: true },
                want_blocked_after_tick: 2,
                want_clear_on_idle: true,
                want_blocked_after_idle: 1,
            },
            Case {
                name: "already blocked worker does not increment blocked count again",
                start: Start::Blocked,
                now_ms: 10_000,
                timeout: Duration::from_secs(2),
                initial_blocked: 1,
                total_workers: 2,
                want_escalation: Escalation::None,
                want_blocked_after_tick: 1,
                want_clear_on_idle: true,
                want_blocked_after_idle: 0,
            },
        ];

        let t0 = Instant::now();
        for case in cases {
            let blocked = AtomicUsize::new(case.initial_blocked);
            let (mut unit, tick_now) = match case.start {
                Start::Idle => (Unit::Idle, t0),
                Start::Active(since) => (Unit::Active(since), t0),
                Start::InterruptedAgo(elapsed) => (Unit::Interrupted(t0), t0 + elapsed),
                Start::Blocked => (Unit::Blocked, t0),
            };
            let got = unit.check_deadline(
                case.now_ms,
                tick_now,
                case.timeout,
                &blocked,
                case.total_workers,
            );
            assert_eq!(got, case.want_escalation, "{}", case.name);
            assert_eq!(
                blocked.load(Relaxed),
                case.want_blocked_after_tick,
                "{}",
                case.name
            );
            let cleared = unit.replace(Unit::Idle, &blocked);
            assert_eq!(cleared, case.want_clear_on_idle, "{}", case.name);
            assert_eq!(
                blocked.load(Relaxed),
                case.want_blocked_after_idle,
                "{}",
                case.name
            );
        }
    }
}
