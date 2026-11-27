use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;
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
    signal_result(unsafe { rapira_watchdog_block_timer() })
}

/// Call on the main thread after PHP module startup and before any pool starts.
pub(crate) fn reserve_wake_signal() -> io::Result<()> {
    signal_result(unsafe { rapira_watchdog_install() })
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

/// True when the calling PHP worker has a request time limit.
pub fn enabled() -> bool {
    LOCAL.with_borrow(Option::is_some)
}

pub(crate) fn wake() -> Option<watch::Receiver<bool>> {
    LOCAL.with_borrow(|slot| Some(slot.as_ref()?.lock().unwrap().wake.subscribe()))
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
                    crate::exit::exit(124);
                }
            }
            _ => {}
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
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("join the watchdog");
    }
}
