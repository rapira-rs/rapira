use anyhow::Context as _;
use crossbeam_channel::{Receiver, Select, Sender, TryRecvError, bounded, never};
use rapira_config::{ExecutionClock, Mode, PoolSettings};
use std::ffi::{CStr, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};
use tracing::error;

use crate::engine::Workers;
use crate::plugin::Plugin;
use crate::pool::{Event, Pool};
use crate::work::{Entry, Idle, Intake, Unit, Units};
use crate::worker::Worker;
use crate::{
    RAPIRA_MODE_CLASSIC, RAPIRA_MODE_DISPATCHER, RAPIRA_MODE_WORKER, php_execute_script,
    rapira_plugin, rapira_thread_end, rapira_thread_start, zend_destroy_file_handle,
    zend_file_handle, zend_stream_init_filename,
};

pub(crate) struct UnitRx {
    pub(crate) rx: Receiver<Entry>,
    idle: Arc<Idle>,
    pool: Arc<Pool>,
    control: Receiver<()>,
    workers: Arc<Workers>,
}

/// Dropping the sender wakes every boot retry when a worker leaves service or the intake closes.
#[derive(Default)]
pub(crate) struct BootRetry(Mutex<Option<(Sender<()>, Receiver<()>)>>);

impl BootRetry {
    fn changes(&self) -> Receiver<()> {
        let mut channel = self.0.lock().unwrap();
        let (_, rx) = channel.get_or_insert_with(|| bounded(0));
        rx.clone()
    }

    pub(crate) fn wake(&self) {
        self.0.lock().unwrap().take();
    }
}

/// An idle worker returns unused pages after this wait.
const IDLE_PURGE_AFTER: Duration = Duration::from_secs(2);
/// An idle pull checks for a pending OPcache restart at this interval.
pub(crate) const OPCACHE_POLL: Duration = Duration::from_secs(1);

impl UnitRx {
    pub(crate) fn new(units: Units, workers: Arc<Workers>) -> (Self, Sender<()>) {
        let (tx, control) = bounded(1);
        (
            Self {
                rx: units.rx,
                idle: units.idle,
                pool: units.pool,
                control,
                workers,
            },
            tx,
        )
    }

    pub(crate) fn wake_workers(&self) {
        self.workers.wake();
    }

    /// Takes what is ready. A reload comes before a queued unit.
    fn take(&self, units: &Receiver<Entry>) -> Option<Pull> {
        match self.control.try_recv() {
            Ok(()) => return Some(Pull::Reload),
            Err(TryRecvError::Disconnected) => return Some(Pull::Closed),
            Err(TryRecvError::Empty) => {}
        }
        match units.try_recv() {
            Ok(entry) => {
                self.pool.limits.release(entry.budget_bytes);
                Some(Pull::Unit(entry.unit))
            }
            Err(TryRecvError::Disconnected) => Some(Pull::Closed),
            Err(TryRecvError::Empty) => None,
        }
    }

    /// A wait with a timeout above zero needs the queue of the pool as `units`, because the submit wakes the workers of that queue.
    pub(crate) fn recv(&self, units: &Receiver<Entry>, timeout: Duration) -> Pull {
        if let Some(pull) = self.take(units) {
            if self.idle.has_waiters() {
                let rem = units.len();
                if rem > 0 {
                    self.idle.wake_one(rem);
                }
            }
            return pull;
        }
        if timeout.is_zero() {
            return Pull::NoUnit;
        }
        let deadline = Instant::now().checked_add(timeout);
        let me = thread::current();
        loop {
            self.idle.enter(&me);
            // A unit can arrive between the last check and the entry.
            let mut got = self.take(units);
            let mut expired = false;
            if got.is_none() {
                match deadline {
                    None => thread::park(),
                    Some(deadline) => {
                        thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
                        expired = Instant::now() >= deadline;
                    }
                }
            }
            self.idle.leave(&me);
            // A woken worker takes the unit, the reload or the close that caused the wake.
            if got.is_none() {
                got = self.take(units);
            }
            match got {
                Some(pull) => {
                    // Hand off to another waiting worker if more units remain queued after our leave decremented waking.
                    if self.idle.has_waiters() {
                        let rem = units.len();
                        if rem > 0 {
                            self.idle.wake_one(rem);
                        }
                    }
                    return pull;
                }
                None if expired => return Pull::NoUnit,
                None => {}
            }
        }
    }

    /// A failed-boot retry takes a queued unit only while no worker in the pool can serve it.
    pub(crate) fn pull_to_shed(&self, deadline: Instant, retry: &Weak<BootRetry>) -> Pull {
        let disabled = never();
        idle_wait(deadline.saturating_duration_since(Instant::now()), |t| {
            let until = deadline.min(Instant::now() + t);
            while Instant::now() < until {
                // Subscribe before checking readiness so the last serving worker can wake this wait.
                let Some(changes) = retry.upgrade().map(|retry| retry.changes()) else {
                    return Pull::Closed;
                };
                let ready = self.pool.ready();
                let units = if ready { &disabled } else { &self.rx };
                let got = self.recv(units, Duration::ZERO);
                if !matches!(got, Pull::NoUnit) {
                    return got;
                }
                // Wait without taking a unit. A worker can become ready before the next unit arrives.
                let mut wait = Select::new();
                wait.recv(&self.control);
                wait.recv(&changes);
                wait.recv(units);
                if wait.ready_deadline(until).is_err() {
                    break;
                }
            }
            Pull::NoUnit
        })
    }
}

pub(crate) fn idle_wait(timeout: Duration, select: impl Fn(Duration) -> Pull) -> Pull {
    let first = timeout.min(IDLE_PURGE_AFTER);
    let got = select(first);
    if matches!(got, Pull::NoUnit) && first == IDLE_PURGE_AFTER {
        // SAFETY: mimalloc can collect on the thread that owns the idle heap.
        unsafe { libmimalloc_sys::mi_collect(true) };
        return select(timeout - first);
    }
    got
}

/// The jitter spreads the recycles across workers.
fn effective_quota(max_requests: u64, index: usize) -> u64 {
    if max_requests == 0 {
        return 0;
    }
    use std::hash::{BuildHasher, RandomState};
    let grace = (max_requests / 2).max(1);
    max_requests.saturating_add(1 + RandomState::new().hash_one(index) % grace)
}

#[derive(Clone, Copy)]
struct ThreadPlugin {
    mode: c_int,
    record: *const rapira_plugin,
    name: &'static str,
    cpu_clock: bool,
}

// SAFETY: the record is immutable from MINIT until all worker threads end.
unsafe impl Send for ThreadPlugin {}

impl ThreadPlugin {
    unsafe fn enter(self) {
        unsafe {
            rapira_thread_start(
                self.mode,
                self.record,
                self.name.as_ptr().cast(),
                self.name.len(),
                self.cpu_clock,
            )
        };
    }
}

impl crate::Engine {
    /// Start the PHP threads and return the submit end of their queue.
    ///
    /// # Safety
    /// The plugin's dispatcher record must stay immutable until engine shutdown.
    pub unsafe fn start_pool(
        &mut self,
        settings: PoolSettings,
        plugin: &dyn Plugin,
        io: &tokio::runtime::Handle,
    ) -> anyhow::Result<Intake> {
        let name = plugin.name();
        let mode = settings.mode;
        let worker_init = plugin.worker_init();
        let plugin = ThreadPlugin {
            name,
            mode: match mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker => RAPIRA_MODE_WORKER,
                Mode::Dispatcher => RAPIRA_MODE_DISPATCHER,
            } as c_int,
            record: plugin.dispatcher(),
            cpu_clock: settings.execution_clock == ExecutionClock::Cpu,
        };
        let pool = Arc::new(Pool::new(name, settings.workers));
        self.pools.push(Arc::clone(&pool));
        let (intake, rx) = Intake::new(Arc::clone(&pool));
        let env = Arc::new(crate::env::PoolEnv::new(&settings.env));
        for index in 0..settings.workers {
            let (units, control) = UnitRx::new(rx.clone(), Arc::clone(&self.workers));
            let io = io.clone();
            let init = Arc::clone(&worker_init);
            let boot_retry = Arc::downgrade(&intake.boot_retry);
            let pool = Arc::clone(&pool);
            let opcache = Arc::clone(&self.opcache);
            let env = Arc::clone(&env);
            let entrypoint = settings.entrypoint.clone();
            let max_requests = if mode == Mode::Classic {
                0
            } else {
                effective_quota(settings.max_requests, index)
            };
            let builder = thread::Builder::new()
                .name(format!("rapira-{name}-{index}"))
                .stack_size(8 << 20);
            self.workers
                .spawn(builder, control, move || {
                    let _entered = io.enter();
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: MINIT has finished. This is the first PHP call on this thread.
                        unsafe { plugin.enter() };
                        let guard = unsafe {
                            crate::watchdog::enter(
                                Arc::clone(&pool),
                                index,
                                settings.request_terminate_timeout,
                            )
                        };
                        pool.bind(index);
                        init();
                        crate::env::enter_pool(env);
                        let worker = Worker::new(
                            units,
                            Arc::clone(&pool),
                            index,
                            opcache,
                            max_requests,
                        );
                        match mode {
                            Mode::Classic => worker.run_classic(&entrypoint),
                            Mode::Worker | Mode::Dispatcher => {
                                worker.run_resident(
                                    &entrypoint,
                                    settings.max_boot_failures,
                                    &boot_retry,
                                );
                            }
                        }
                        pool.record_worker(index, Event::Stopped);
                        drop(guard);
                        // SAFETY: the last request ended and the watchdog released the PHP globals.
                        unsafe { rapira_thread_end() };
                    }));
                    if let Err(cause) = result {
                        let message = cause
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| cause.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("unknown panic");
                        error!(target: "rapira", "{name} pool: worker {index} panicked: {message}; exiting with code 101");
                        crate::exit::exit(101);
                    }
                })
                .with_context(|| format!("spawning {name} worker {index}"))?;
        }
        if !settings.request_terminate_timeout.is_zero() && self.watchdog.is_none() {
            self.watchdog = Some(crate::watchdog::start().context("starting the watchdog")?);
        }
        Ok(intake)
    }
}

/// # Safety
/// Requires an active request. `php_execute_script` catches a bailout itself.
pub(crate) unsafe fn run_script(script: &CStr) -> bool {
    unsafe {
        let mut fh: zend_file_handle = std::mem::zeroed();
        zend_stream_init_filename(&mut fh, script.as_ptr());
        fh.primary_script = true;
        let ok: bool = php_execute_script(&mut fh);
        zend_destroy_file_handle(&mut fh);
        ok
    }
}

/// How long a pull waits for a unit.
#[derive(Clone, Copy)]
pub(crate) enum Wait {
    Forever,
    For(Duration),
    Never,
}

/// The result of a pull.
pub(crate) enum Pull {
    Unit(Unit),
    /// A reload ends this app instance or changes the entrypoint of the classic loop.
    Reload,
    /// The app instance takes no more units, because its quota is used up or the intake closed.
    Closed,
    /// No unit arrived within the wait.
    NoUnit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::Held;

    struct Probe;

    impl Held for Probe {
        fn settled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> bool {
            false
        }
        fn discard(&mut self) {}
        fn lost(&mut self) {}
        fn shed(self: Box<Self>) {}
        fn class(&self) -> *mut crate::zend_class_entry {
            std::ptr::null_mut()
        }
    }

    #[test]
    fn reload_pull_hands_off_queued_unit_to_parked_worker() {
        struct Case {
            name: &'static str,
            park_before_reload: bool,
        }
        let cases = [
            Case {
                name: "immediate take returning reload wakes parked worker for queued unit",
                park_before_reload: false,
            },
            Case {
                name: "parked worker woken into reload hands off queued unit to other parked worker",
                park_before_reload: true,
            },
        ];

        for case in cases {
            let pool = Arc::new(Pool::new("test", 2));
            let (intake, units) = Intake::new(Arc::clone(&pool));
            let idle = Arc::clone(&units.idle);
            let (w1, tx1) = UnitRx::new(units.clone(), Arc::default());
            let (w2, _tx2) = UnitRx::new(units, Arc::default());
            let (tx_u, rx_u) = bounded(4);
            let rx_u_w2 = rx_u.clone();
            let (got_tx, got_rx) = bounded(1);

            let t2 = thread::spawn(move || {
                let pull = w2.recv(&rx_u_w2, Duration::from_secs(2));
                let _ = got_tx.send(matches!(pull, Pull::Unit(_)));
            });

            let spin_deadline = Instant::now() + Duration::from_secs(2);
            while !idle.has_waiters() {
                assert!(
                    Instant::now() < spin_deadline,
                    "{}: w2 did not park",
                    case.name
                );
                thread::yield_now();
            }

            let entry = Entry {
                unit: Unit::Dispatcher(Box::new(Probe)),
                budget_bytes: 0,
            };

            if case.park_before_reload {
                let rx_u_w1 = rx_u.clone();
                let (w1_done_tx, w1_done_rx) = bounded(1);
                let t1 = thread::spawn(move || {
                    let pull = w1.recv(&rx_u_w1, Duration::from_secs(2));
                    let _ = w1_done_tx.send(matches!(pull, Pull::Reload));
                });
                thread::sleep(Duration::from_millis(20));
                tx1.try_send(()).unwrap();
                tx_u.send(entry).unwrap();
                idle.wake_one(1);
                assert!(
                    w1_done_rx.recv_timeout(Duration::from_millis(500)).unwrap(),
                    "{}: w1 should pull Reload",
                    case.name
                );
                t1.join().unwrap();
            } else {
                tx1.try_send(()).unwrap();
                tx_u.send(entry).unwrap();
                assert!(
                    matches!(w1.recv(&rx_u, Duration::ZERO), Pull::Reload),
                    "{}: w1 should pull Reload",
                    case.name
                );
            }

            let w2_got_unit = got_rx.recv_timeout(Duration::from_millis(500));
            drop(tx_u);
            drop(intake);
            t2.join().unwrap();
            assert_eq!(w2_got_unit.ok(), Some(true), "{}", case.name);
        }
    }
}
