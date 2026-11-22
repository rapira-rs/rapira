use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rapira_scoreboard::{Pool, now_millis};
use tokio::sync::watch;
use tracing::{error, warn};

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
    fn rapira_watchdog_mask(how: c_int, wake: bool) -> c_int;
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

/// Call on the main thread before it starts other threads. Only PHP threads unblock these signals.
pub fn block_signals() -> io::Result<()> {
    signal_result(unsafe { rapira_watchdog_mask(libc::SIG_BLOCK, true) })
}

fn install_handler() -> io::Result<()> {
    static RESULT: OnceLock<c_int> = OnceLock::new();
    signal_result(*RESULT.get_or_init(|| unsafe { rapira_watchdog_install() }))
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

/// The guard must stay on the PHP thread that owns the flags.
#[must_use]
pub(crate) struct Guard(PhantomData<Rc<()>>);

/// # Safety
/// Call once after this worker's `rapira_thread_start`. Drop the guard before `rapira_thread_end`.
pub(crate) unsafe fn enter(pool: Arc<Pool>, index: usize, timeout: Duration) -> Guard {
    let enabled = !timeout.is_zero();
    if enabled {
        install_handler().expect("install the watchdog signal handler");
    }
    signal_result(unsafe { rapira_watchdog_mask(libc::SIG_UNBLOCK, enabled) })
        .expect("unblock the PHP thread signals");
    if enabled {
        let slot = Arc::new(Mutex::new(Worker {
            pool,
            index,
            timeout,
            interrupt: unsafe { rapira_watchdog_capture() },
            wake: watch::Sender::new(false),
            unit: Unit::Idle,
        }));
        WORKERS.lock().unwrap().push(slot.clone());
        LOCAL.set(Some(slot));
    }
    Guard(PhantomData)
}

impl Drop for Guard {
    fn drop(&mut self) {
        clear();
        if let Some(slot) = LOCAL.take() {
            // A tick keeps this lock until its last access to the flags and the pthread ID.
            WORKERS.lock().unwrap().retain(|s| !Arc::ptr_eq(s, &slot));
        }
        block_signals().expect("block the PHP thread signals before shutdown");
    }
}

pub(crate) fn arm(last_activity_ms: u64) {
    set_unit(Unit::Active(last_activity_ms));
}

pub(crate) fn clear() {
    set_unit(Unit::Idle);
}

/// True when the calling PHP worker has a request time limit.
pub fn enabled() -> bool {
    LOCAL.with_borrow(Option::is_some)
}

pub(crate) fn wake() -> Option<watch::Receiver<bool>> {
    LOCAL.with_borrow(|slot| Some(slot.as_ref()?.lock().unwrap().wake.subscribe()))
}

/// # Safety
/// The calling PHP thread paused this timer. A watchdog interrupt must keep the process timer disarmed.
pub(crate) unsafe fn resume_timer(saved: &crate::rapira_timer) {
    LOCAL.with_borrow(|slot| {
        let worker = slot.as_ref().map(|slot| slot.lock().unwrap());
        if worker.as_ref().is_none_or(|w| !w.unit.interrupted()) {
            unsafe { crate::rapira_timer_resume(saved) };
        }
    });
}

fn set_unit(unit: Unit) {
    LOCAL.with_borrow(|slot| {
        if let Some(slot) = slot {
            let mut worker = slot.lock().unwrap();
            if matches!(worker.unit, Unit::Blocked) {
                worker.pool.blocked.fetch_sub(1, Relaxed);
            }
            if worker.unit.interrupted() {
                // The previous fire has returned. Withdraw its timeout before the next unit or boot.
                unsafe { rapira_watchdog_clear(&worker.interrupt) };
                worker.wake.send_replace(false);
            }
            worker.unit = unit;
        }
    });
}

impl Worker {
    fn tick(&mut self) {
        let (pool, index, timeout) = (self.pool.name, self.index, self.timeout);
        match self.unit {
            Unit::Active(since) if Duration::from_millis(now_millis() - since) >= timeout => {
                unsafe { rapira_watchdog_fire(&self.interrupt) };
                self.unit = Unit::Interrupted(Instant::now());
                self.wake.send_replace(true);
                warn!(target: "rapira", "{pool}.pool.request_terminate_timeout_secs exceeded on worker {index} ({timeout:?}); interrupting the request");
            }
            Unit::Interrupted(at) if at.elapsed() >= GRACE => {
                self.unit = Unit::Blocked;
                error!(target: "rapira", "{pool} worker {index} is blocked {GRACE:?} after its interrupt; the unit will end when its native call returns");
                if self.pool.blocked.fetch_add(1, Relaxed) + 1 == self.pool.workers.len() {
                    error!(target: "rapira", "every worker of pool {pool} is blocked; exiting with code 124");
                    unsafe { libc::_exit(124) };
                }
            }
            _ => {}
        }
    }
}

pub struct Watchdog {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

/// Start after worker startup when at least one pool has a request timeout.
pub fn start() -> io::Result<Watchdog> {
    install_handler()?;
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
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("join the watchdog");
    }
}
