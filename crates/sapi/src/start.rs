use anyhow::Context as _;
use crossbeam_channel::{Receiver, Select, Sender, bounded, never, select_biased};
use rapira_config::{Mode, PoolSettings};
use rapira_scoreboard::Pool as PoolStats;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{error, info};

use crate::plugin::Plugin;
use crate::quota;
use crate::rapira_worker::rapira_worker;
use crate::scoreboard::{Event, sb_set, sb_update};
use crate::work::{Entry, Intake, Unit};
use crate::{
    RAPIRA_MODE_CLASSIC, RAPIRA_MODE_DISPATCHER, RAPIRA_MODE_WORKER,
    classic_worker::classic_worker, php_execute_script, rapira_plugin, rapira_thread_end,
    rapira_thread_start, zend_destroy_file_handle, zend_file_handle, zend_stream_init_filename,
};

thread_local! {
    static UNIT_RX: RefCell<Option<UnitRx>> = const { RefCell::new(None) };
    /// The app took at least one unit in this cycle.
    pub(crate) static RECEIVED: Cell<bool> = const { Cell::new(false) };
    /// A pull of this cycle saw the intake closed.
    pub(crate) static CLOSED_SEEN: Cell<bool> = const { Cell::new(false) };
}

struct UnitRx {
    rx: Receiver<Entry>,
    control: Receiver<()>,
    stats: Arc<PoolStats>,
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
const OPCACHE_POLL: Duration = Duration::from_secs(1);

impl UnitRx {
    fn new(rx: Receiver<Entry>, stats: Arc<PoolStats>) -> Self {
        let (tx, control) = bounded(1);
        crate::reload::register(tx);
        Self { rx, control, stats }
    }

    fn recv(&self, units: &Receiver<Entry>, timeout: Duration) -> Pull {
        select_biased! {
            recv(self.control) -> msg => msg.map_or(Pull::Closed, |()| Pull::Reload),
            recv(units) -> msg => msg.map_or(Pull::Closed, |entry| {
                self.stats.pending.fetch_sub(1, Ordering::Relaxed);
                Pull::Unit(entry.unit)
            }),
            default(timeout) => Pull::NoUnit,
        }
    }
}

fn idle_wait(timeout: Duration, select: impl Fn(Duration) -> Pull) -> Pull {
    let first = timeout.min(IDLE_PURGE_AFTER);
    let got = select(first);
    if matches!(got, Pull::NoUnit) && first == IDLE_PURGE_AFTER {
        // SAFETY: mimalloc can collect on the thread that owns the idle heap.
        unsafe { libmimalloc_sys::mi_collect(true) };
        return select(timeout - first);
    }
    got
}

pub struct Pool {
    workers: Vec<JoinHandle<()>>,
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
}

// SAFETY: the record is null or immutable from MINIT until all worker threads end.
unsafe impl Send for ThreadPlugin {}

impl ThreadPlugin {
    unsafe fn enter(self) {
        unsafe {
            rapira_thread_start(
                self.mode,
                self.record,
                self.name.as_ptr().cast(),
                self.name.len(),
            )
        };
    }
}

impl Pool {
    /// Start the PHP threads and return the submit end of their queue.
    ///
    /// # Safety
    /// Start on the engine thread after MINIT. The dispatcher record must stay immutable until all workers have joined. Keep the engine running until then. A spawn error can leave workers running.
    pub unsafe fn start(
        settings: PoolSettings,
        stats: Arc<PoolStats>,
        plugin: &dyn Plugin,
    ) -> anyhow::Result<(Self, Intake)> {
        let name = plugin.name();
        let mode = settings.mode;
        let plugin = ThreadPlugin {
            name,
            mode: match mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker => RAPIRA_MODE_WORKER,
                Mode::Dispatcher => RAPIRA_MODE_DISPATCHER,
            } as c_int,
            record: if mode == Mode::Dispatcher {
                plugin.dispatcher()
            } else {
                std::ptr::null()
            },
        };
        let (intake, rx) = Intake::new(Arc::clone(&stats));
        let env = Arc::new(settings.env);
        let server_env = crate::context::intern_server_env(&env);
        let mut workers = Vec::with_capacity(settings.workers);
        for index in 0..settings.workers {
            let units = UnitRx::new(rx.clone(), Arc::clone(&stats));
            let boot_retry = Arc::downgrade(&intake.boot_retry);
            let stats = Arc::clone(&stats);
            let server_env = Arc::clone(&server_env);
            let env = Arc::clone(&env);
            let entrypoint = settings.entrypoint.clone();
            let max_requests = if mode == Mode::Classic {
                0
            } else {
                effective_quota(settings.max_requests, index)
            };
            let worker = thread::Builder::new()
                .name(format!("rapira-{name}-{index}"))
                .stack_size(8 << 20)
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: MINIT has finished. This is the first PHP call on this thread.
                        unsafe { plugin.enter() };
                        let guard = unsafe {
                            crate::watchdog::enter(
                                Arc::clone(&stats),
                                index,
                                settings.request_terminate_timeout,
                            )
                        };
                        sb_set(Arc::clone(&stats), index);
                        UNIT_RX.set(Some(units));
                        crate::context::enter_server_env(server_env);
                        crate::env::enter_pool(env);
                        quota::install(max_requests);
                        match mode {
                            Mode::Classic => classic_worker(&entrypoint),
                            Mode::Worker | Mode::Dispatcher => {
                                rapira_worker(&entrypoint, settings.max_boot_failures, &stats, index, &boot_retry);
                            }
                        }
                        sb_update(Event::Stopped);
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
                        unsafe { libc::_exit(101) };
                    }
                })
                .with_context(|| format!("spawning {name} worker {index}"))?;
            workers.push(worker);
        }
        Ok((Self { workers }, intake))
    }

    pub fn is_finished(&self) -> bool {
        self.workers.iter().all(JoinHandle::is_finished)
    }

    pub fn join(self) {
        for worker in self.workers {
            worker
                .join()
                .expect("the worker catches a panic before it exits");
        }
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

/// How long a pull waits for a unit, or a park for its event.
#[derive(Clone, Copy)]
pub enum Wait {
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

/// A timed pull keeps one deadline across cancelled units. PHP becomes active again even when a poll finds no unit.
pub(crate) fn pull(wait: Wait) -> Pull {
    UNIT_RX.with_borrow(|slot| {
        let Some(unit_r) = slot.as_ref() else {
            CLOSED_SEEN.set(true);
            return Pull::Closed;
        };
        let deadline = match wait {
            Wait::Never => Some(Instant::now()),
            Wait::For(t) => Some(Instant::now() + t),
            Wait::Forever => None,
        };
        crate::rapira_worker::reload_for_opcache_restart();
        loop {
            // The app instance took its last unit; the worker stays for the next one.
            if quota::exhausted() {
                return Pull::Closed;
            }
            if crate::rapira_worker::opcache_restart_ran() {
                info!(target: "rapira", "the OPcache restart has run; restarting the app");
                quota::exhaust();
                sb_update(Event::Active);
                return Pull::Closed;
            }
            sb_update(Event::Idle);
            let poll = crate::rapira_worker::OPCACHE_WAIT
                .get()
                .map(|_| OPCACHE_POLL);
            let left = deadline.map_or(Duration::MAX, |d| {
                d.saturating_duration_since(Instant::now())
            });
            let got = idle_wait(poll.map_or(left, |p| p.min(left)), |t| {
                unit_r.recv(&unit_r.rx, t)
            });
            match &got {
                Pull::Unit(unit) if unit.cancelled() => {
                    sb_update(Event::Shed);
                    continue;
                }
                Pull::Unit(_) => RECEIVED.set(true),
                Pull::Reload => quota::exhaust(),
                Pull::Closed => CLOSED_SEEN.set(true),
                Pull::NoUnit if poll.is_some() && deadline.is_none_or(|d| Instant::now() < d) => {
                    continue;
                }
                Pull::NoUnit => {}
            }
            sb_update(Event::Active);
            return got;
        }
    })
}

/// A retry takes a queued unit only while no worker can serve it.
pub(crate) fn pull_unit_to_shed(deadline: Instant, retry: &Weak<BootRetry>) -> Pull {
    UNIT_RX.with_borrow(|slot| {
        let Some(rx) = slot else { return Pull::Closed };
        let disabled = never();
        idle_wait(deadline.saturating_duration_since(Instant::now()), |t| {
            let until = deadline.min(Instant::now() + t);
            while Instant::now() < until {
                // Subscribe before checking readiness so the last serving worker can wake this wait.
                let Some(changes) = retry.upgrade().map(|retry| retry.changes()) else {
                    return Pull::Closed;
                };
                let units = if rx.stats.ready() { &disabled } else { &rx.rx };
                let got = rx.recv(units, Duration::ZERO);
                if !matches!(got, Pull::NoUnit) {
                    return got;
                }
                // Wait without taking a unit. A worker can become ready before the next unit arrives.
                let mut wait = Select::new();
                wait.recv(&rx.control);
                wait.recv(&changes);
                wait.recv(units);
                if wait.ready_deadline(until).is_err() {
                    break;
                }
            }
            Pull::NoUnit
        })
    })
}

pub(crate) fn pending_depth() -> u64 {
    UNIT_RX.with_borrow(|slot| {
        slot.as_ref()
            .map_or(0, |unit_r| unit_r.stats.pending.load(Ordering::Relaxed))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Instant;

    use super::*;
    use crate::scoreboard::sb_set;
    use crate::work::Held;

    /// A unit on the intake. `cancelled` is true when its client left.
    struct Probe {
        cancelled: bool,
    }

    impl Held for Probe {
        fn settled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> bool {
            self.cancelled
        }
        fn discard(&mut self) {}
        fn lost(&mut self) {}
        fn shed(self: Box<Self>) {}
        fn class(&self) -> *mut crate::zend_class_entry {
            std::ptr::null_mut()
        }
    }

    /// One worker with a private control channel and an in-memory intake.
    struct TestWorker {
        stats: Arc<PoolStats>,
        control: crossbeam_channel::Sender<()>,
        intake: Intake,
    }

    fn worker(max_requests: u64) -> TestWorker {
        let stats = Arc::new(PoolStats::new("http", 1));
        sb_set(Arc::clone(&stats), 0);
        quota::install(max_requests);
        RECEIVED.set(false);
        CLOSED_SEEN.set(false);
        let (intake, rx) = Intake::new(Arc::clone(&stats));
        let (tx, control) = bounded(1);
        UNIT_RX.set(Some(UnitRx {
            rx,
            control,
            stats: Arc::clone(&stats),
        }));
        TestWorker {
            stats,
            intake,
            control: tx,
        }
    }

    fn kind(pull: &Pull) -> &'static str {
        match pull {
            Pull::Unit(_) => "unit",
            Pull::Reload => "reload",
            Pull::Closed => "closed",
            Pull::NoUnit => "no unit",
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_reload_reaches_every_worker_of_the_pool() {
        struct Case {
            name: &'static str,
            queued: usize,
        }
        let cases = [
            Case {
                name: "both workers wait on an empty intake",
                queued: 0,
            },
            Case {
                name: "a full intake leaves room for every reload",
                queued: crate::work::INTAKE_SLOTS,
            },
        ];
        for case in cases {
            let stats = Arc::new(PoolStats::new("http", 2));
            let (intake, rx) = Intake::new(Arc::clone(&stats));
            let workers = [
                UnitRx::new(rx.clone(), Arc::clone(&stats)),
                UnitRx::new(rx, Arc::clone(&stats)),
            ];
            for _ in 0..case.queued {
                intake
                    .submit(Unit::Dispatcher(Box::new(Probe { cancelled: false })), 0)
                    .await
                    .unwrap();
            }
            crate::reload::wake_workers();
            for (index, worker) in workers.into_iter().enumerate() {
                sb_set(Arc::clone(&stats), index);
                quota::install(0);
                UNIT_RX.set(Some(worker));
                assert_eq!(
                    kind(&pull(Wait::Never)),
                    "reload",
                    "{}: worker {index}",
                    case.name
                );
                assert_eq!(
                    kind(&pull(Wait::Never)),
                    "closed",
                    "{}: same app",
                    case.name
                );
            }
            assert_eq!(pending_depth(), case.queued as u64, "{}", case.name);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_pull_applies_the_app_rules() {
        /// What enters the intake before the pull, in order.
        enum Feed {
            Unit,
            Cancelled,
            /// A CGI unit whose client waits.
            Cgi,
            /// A CGI unit whose client left.
            CgiGone,
            Reload,
            /// The app served one unit, so the quota ticks.
            Served,
            /// Every intake clone is dropped.
            Close,
        }
        struct Counts {
            handled: u64,
            errors: u64,
            pending: u64,
        }
        struct Case {
            name: &'static str,
            max_requests: u64,
            feed: &'static [Feed],
            wait: Wait,
            want: &'static str,
            received: bool,
            closed_seen: bool,
            exhausted: bool,
            counts: Counts,
            /// The pull returns within this time.
            max_elapsed: Duration,
        }
        const NO_LIMIT: Duration = Duration::from_secs(5);
        const TIMED: Duration = Duration::from_millis(400);
        let cases = [
            Case {
                name: "a unit is pulled",
                max_requests: 0,
                feed: &[Feed::Unit],
                wait: Wait::Never,
                want: "unit",
                received: true,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a cancelled unit is skipped and counted as shed",
                max_requests: 0,
                feed: &[Feed::Cancelled, Feed::Unit],
                wait: Wait::Never,
                want: "unit",
                received: true,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 1,
                    errors: 1,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a CGI unit is pulled",
                max_requests: 0,
                feed: &[Feed::Cgi],
                wait: Wait::Never,
                want: "unit",
                received: true,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a CGI unit whose client left is skipped and counted as shed",
                max_requests: 0,
                feed: &[Feed::CgiGone, Feed::Cgi],
                wait: Wait::Never,
                want: "unit",
                received: true,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 1,
                    errors: 1,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a cancelled unit alone gives no unit",
                max_requests: 0,
                feed: &[Feed::Cancelled],
                wait: Wait::Never,
                want: "no unit",
                received: false,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 1,
                    errors: 1,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a cancelled unit does not use the quota",
                max_requests: 1,
                feed: &[Feed::Cancelled, Feed::Unit],
                wait: Wait::Never,
                want: "unit",
                received: true,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 1,
                    errors: 1,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a reload closes the app before it takes a queued unit",
                max_requests: 10,
                feed: &[Feed::Unit, Feed::Reload],
                wait: Wait::Never,
                want: "reload",
                received: false,
                closed_seen: false,
                exhausted: true,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 1,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a classic reload returns to the loop before it takes the next unit",
                max_requests: 0,
                feed: &[Feed::Cgi, Feed::Reload],
                wait: Wait::Forever,
                want: "reload",
                received: false,
                closed_seen: false,
                exhausted: true,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 1,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "an exhausted quota pulls nothing",
                max_requests: 1,
                feed: &[Feed::Served, Feed::Unit],
                wait: Wait::Never,
                want: "closed",
                received: false,
                closed_seen: false,
                exhausted: true,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 1,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "a closed intake ends a blocking pull with a live control sender",
                max_requests: 0,
                feed: &[Feed::Close],
                wait: Wait::Forever,
                want: "closed",
                received: false,
                closed_seen: true,
                exhausted: false,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 0,
                },
                max_elapsed: NO_LIMIT,
            },
            Case {
                name: "an empty intake ends a timed pull",
                max_requests: 0,
                feed: &[],
                wait: Wait::For(TIMED),
                want: "no unit",
                received: false,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 0,
                    errors: 0,
                    pending: 0,
                },
                max_elapsed: TIMED + TIMED / 2,
            },
            Case {
                name: "a timed pull keeps one deadline across a skipped unit",
                max_requests: 0,
                feed: &[Feed::Cancelled],
                wait: Wait::For(TIMED),
                want: "no unit",
                received: false,
                closed_seen: false,
                exhausted: false,
                counts: Counts {
                    handled: 1,
                    errors: 1,
                    pending: 0,
                },
                max_elapsed: TIMED + TIMED / 2,
            },
        ];
        for case in cases {
            let TestWorker {
                stats,
                control,
                intake,
            } = worker(case.max_requests);
            let mut intake = Some(intake);
            // The clients of the CGI units that wait.
            let mut clients = Vec::new();
            for feed in case.feed {
                match feed {
                    Feed::Unit | Feed::Cancelled => {
                        let cancelled = matches!(feed, Feed::Cancelled);
                        intake
                            .as_ref()
                            .unwrap()
                            .submit(Unit::Dispatcher(Box::new(Probe { cancelled })), 0)
                            .await
                            .unwrap();
                    }
                    Feed::Cgi | Feed::CgiGone => {
                        let (tx, rx) = tokio::sync::mpsc::channel(8);
                        if matches!(feed, Feed::Cgi) {
                            clients.push(rx);
                        }
                        let mut ctx = crate::types::tests::context();
                        ctx.sender = Some(tx);
                        intake
                            .as_ref()
                            .unwrap()
                            .submit(Unit::Cgi(Box::new(ctx)), 0)
                            .await
                            .unwrap();
                    }
                    Feed::Reload => control.try_send(()).unwrap(),
                    Feed::Served => quota::tick(),
                    Feed::Close => drop(intake.take()),
                }
            }
            let started = Instant::now();
            let got = pull(case.wait);
            let elapsed = started.elapsed();
            assert_eq!(kind(&got), case.want, "{}", case.name);
            if let Wait::For(wait) = case.wait {
                assert!(
                    elapsed >= wait,
                    "{}: {elapsed:?} is below the wait",
                    case.name
                );
            }
            assert!(
                elapsed < case.max_elapsed,
                "{}: {elapsed:?} is not below {:?}",
                case.name,
                case.max_elapsed
            );
            assert_eq!(
                (RECEIVED.get(), CLOSED_SEEN.get(), quota::exhausted()),
                (case.received, case.closed_seen, case.exhausted),
                "{}: received, closed seen, exhausted",
                case.name
            );
            assert_eq!(
                (
                    stats.workers[0].handled.load(Relaxed),
                    stats.workers[0].errors.load(Relaxed),
                    stats.pending.load(Relaxed),
                ),
                (case.counts.handled, case.counts.errors, case.counts.pending),
                "{}: handled, errors, pending",
                case.name
            );
        }
    }
}
