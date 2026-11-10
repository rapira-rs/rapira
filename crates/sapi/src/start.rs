use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, trace};

use crate::quota;
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, sb_set, sb_update};
use crate::work::{Entry, Intake, Unit};
use crate::{
    classic_worker::classic_worker,
    plugin::{Mode, Plugin, Stopper},
    *,
};

thread_local! {
    static UNIT_RX: RefCell<Option<UnitRx>> = const { RefCell::new(None) };
    /// A pull of this cycle took a unit: a fatal error after that is an app failure, not a boot failure.
    pub(crate) static RECEIVED: Cell<bool> = const { Cell::new(false) };
    /// A pull of this cycle saw the intake closed.
    pub(crate) static CLOSED_SEEN: Cell<bool> = const { Cell::new(false) };
}

struct UnitRx {
    rx: Receiver<Entry>,
    /// The `pending` field of this worker's scoreboard slot.
    pending: &'static AtomicU64,
    /// Classic mode runs every unit in a new cycle, so a reload has no app instance to restart and the pull ignores it.
    classic: bool,
}

/// The idle wait after which the PHP thread calls `mi_collect`. mimalloc purges freed arena slices only from a page free or a collect, so an idle worker keeps the slices of the last burst committed.
const IDLE_PURGE_AFTER: Duration = Duration::from_secs(2);

impl UnitRx {
    /// Waits for one entry, with no time limit when `timeout` is None.
    fn recv(&self, timeout: Option<Duration>) -> Result<Entry, RecvTimeoutError> {
        let first = timeout.map_or(IDLE_PURGE_AFTER, |t| t.min(IDLE_PURGE_AFTER));
        let mut got = self.rx.recv_timeout(first);
        if matches!(got, Err(RecvTimeoutError::Timeout)) && first == IDLE_PURGE_AFTER {
            // SAFETY: plain allocator call on this thread; the arena purge is process-wide and locked inside mimalloc.
            unsafe { libmimalloc_sys::mi_collect(true) };
            got = match timeout {
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(t) => self.rx.recv_timeout(t - first),
            };
        }
        got
    }

    /// A pulled unit leaves `pending` and gives back its body bytes.
    fn take(&self, unit: Unit) -> Unit {
        self.pending.fetch_sub(1, Ordering::Relaxed);
        unit
    }
}

pub struct PhpModule {}

impl Drop for PhpModule {
    fn drop(&mut self) {
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
    }
}

pub struct Rapira {
    worker: JoinHandle<()>,
}

/// Jitter avoids lockstep recycling. The hash mixes in the pid because every child inherits the same seed from the pre-fork master.
fn effective_quota(max_requests: u64) -> u64 {
    if max_requests == 0 {
        return 0;
    }
    let grace = (max_requests / 2).max(1);
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    max_requests.saturating_add(1 + (h.finish() % grace))
}

/// The PHP thread of one worker and its intake.
pub struct StartedWorker {
    pub rapira: Rapira,
    /// The intake of this worker. The PHP thread sees it closed once every clone is dropped.
    pub intake: Intake,
}

/// Zend structs are bound by bindgen at build time, so a libphp from another PHP minor is an ABI mismatch (`sapi_startup` handed a differently shaped struct), not a load error.
fn check_linked_php() -> anyhow::Result<()> {
    // SAFETY: php_version_id() returns a compile-time constant and touches no engine state, so this is valid pre-startup.
    let linked = unsafe { php_version_id() };
    // A PHP_VERSION_ID is major * 10000 + minor * 100 + patch, so `/ 100` drops the patch: https://www.php.net/manual/en/function.phpversion.php
    anyhow::ensure!(
        linked / 100 == PHP_VERSION_ID / 100,
        "linked libphp is PHP {} (PHP_VERSION_ID {linked}), but this rapira was built against PHP_VERSION_ID {PHP_VERSION_ID}. \
         Use a libphp from the same PHP minor as the build.",
        linked_php_version()
    );
    Ok(())
}

/// The PHP_VERSION string of the linked libphp.
pub fn linked_php_version() -> String {
    // SAFETY: php_version() returns a static NUL-terminated string and touches no engine state, so this is valid pre-startup.
    unsafe { CStr::from_ptr(php_version()) }
        .to_string_lossy()
        .into_owned()
}

/// MINIT once in the master. Base classes first, then each register function in order.
pub fn boot_master(register: &'static [unsafe extern "C" fn()]) -> anyhow::Result<PhpModule> {
    check_linked_php()?;
    // SAFETY: the master writes them once, before MINIT reads them. register is 'static, so the pointer stays valid. Option<unsafe extern "C" fn()>, the element type bindgen gives, has the layout of the fn pointer.
    unsafe {
        rapira_plugin_registers = register.as_ptr().cast();
        rapira_plugin_register_count = register.len();
    }
    let mut module: _sapi_module_struct = module::build_sapi_module();
    let started: bool = unsafe {
        // The Rust runtime sets SIGPIPE to SIG_IGN before main, so a write to a closed peer returns EPIPE: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/on-broken-pipe.html
        rapira_process_init();
        sapi_startup(&mut module);
        php_module_startup(&mut module, &raw mut rapira_module_entry) == SUCCESS
    };

    // Drop shuts the module down on the failure path too.
    let module = PhpModule {};
    if !started {
        error!(target: "rapira", "php_module_startup failed, shutting down");
        return Err(anyhow::anyhow!("php_module_startup failed"));
    }
    unsafe { rapira_register_post_reader() };
    Ok(module)
}

impl Rapira {
    /// `entrypoint`: the script of every request in classic mode, the worker script otherwise. `max_requests`: the configured value, 0 = unlimited; classic mode gets no quota, the other modes get a jitter.
    /// `slot`: the scoreboard slot this worker reports into. `stopper` stops the plugin when PHP turns unhealthy. `plugin`: the plugin of the pool; in dispatcher mode, its name and its record are the dispatcher surface that PHP sees.
    /// On return, the calling thread blocks SIGPROF, and every thread it starts inherits the block.
    pub fn start_worker(
        mode: Mode,
        entrypoint: PathBuf,
        max_requests: u64,
        slot: &'static rapira_scoreboard::SharedSlot,
        stopper: Stopper,
        plugin: &dyn Plugin,
    ) -> StartedWorker {
        // Classic mode runs every unit in a new cycle, so max_requests has nothing to restart there.
        let max_requests = if mode == Mode::Classic {
            0
        } else {
            effective_quota(max_requests)
        };
        let pending: &'static AtomicU64 = &slot.pending;
        let (intake_tx, intake_rx) = channel::<Entry>();
        let intake = Intake::new(intake_tx, slot);

        crate::context::set_script(&entrypoint);
        // SAFETY: written once, before the PHP thread that reads them exists. The name and the record are static, so the pointers stay valid.
        unsafe {
            crate::rapira_mode = match mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Worker => RAPIRA_MODE_WORKER,
                Mode::Dispatcher => RAPIRA_MODE_DISPATCHER,
            } as c_int;
            if mode == Mode::Dispatcher {
                let name = plugin.name();
                rapira_plugin_current = plugin.dispatcher();
                rapira_plugin_name = name.as_ptr().cast();
                rapira_plugin_name_len = name.len();
            }
        };

        trace!(target: "rapira", "spawning worker thread");
        // PHP sets its stack limit from the stack size of this thread. 8 MiB matches the CLI and FPM main thread under the default RLIMIT_STACK. https://github.com/php/php-src/blob/php-8.5.11/Zend/zend_call_stack.c#L129-L173
        let worker: JoinHandle<()> = thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                sb_set(slot);
                quota::install(max_requests, stopper);
                worker_main(
                    mode,
                    entrypoint,
                    UnitRx {
                        rx: intake_rx,
                        pending,
                        classic: mode == Mode::Classic,
                    },
                )
            })
            .expect("spawn the PHP thread");

        // The max_execution_time timer sends SIGPROF to the process. The PHP thread exists and keeps the signal open; every thread started after this block inherits the block.
        // SAFETY: set is a live sigset_t.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGPROF);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        }

        StartedWorker {
            rapira: Self { worker },
            intake,
        }
    }

    /// Waits for the PHP thread after the plugin stopped. The thread ends once every Intake clone is dropped, and the plugin drops its clone with its runtime. Returns true when PHP turned unhealthy.
    pub fn join(self) -> bool {
        info!(target: "rapira", "shutting down, joining the PHP thread");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !self.worker.is_finished() {
            thread::sleep(Duration::from_millis(20));
        }
        if self.worker.is_finished() {
            let _ = self.worker.join();
        } else {
            error!(target: "rapira", "worker still running after grace; leaving it unjoined");
        }
        quota::unhealthy()
    }
}

/// NTS inits module and request on different threads, so the call stack is re-initialized on this thread: https://github.com/php/php-src/pull/9104
fn worker_main(mode: Mode, entrypoint: PathBuf, rx: UnitRx) {
    UNIT_RX.with_borrow_mut(|slot| *slot = Some(rx));
    loop {
        unsafe {
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match mode {
            Mode::Classic => {
                classic_worker();
                WorkerExit::Closed
            }
            Mode::Worker | Mode::Dispatcher => rapira_worker(&entrypoint),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
    }
}

/// # Safety
/// Requires an active request. `php_execute_script` catches a bailout itself.
pub(crate) unsafe fn run_script(script: &Path) -> bool {
    unsafe {
        let c_script: CString =
            CString::new(script.to_string_lossy().as_bytes()).unwrap_or_default();
        let mut fh: zend_file_handle = std::mem::zeroed();
        zend_stream_init_filename(&mut fh, c_script.as_ptr());
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
    /// The app instance takes no more units: its quota is used up, or the intake closed.
    Closed,
    /// No unit arrived within the wait.
    NoUnit,
}

/// The app pull of every mode. It applies the app rules: the quota, a reload, a cancelled unit and the cycle flags. A timed pull keeps one deadline across the entries it skips.
/// Idle covers only the park: control returns to PHP as Active on every arm, or the master watchdog skips a worker spinning after a no-unit return. A polling worker must refresh last_activity_ms, or the watchdog TERMs it as a stuck request (`Pool::watchdog_tick`).
pub(crate) fn pull(wait: Wait) -> Pull {
    UNIT_RX.with_borrow(|slot| {
        let Some(unit_r) = slot.as_ref() else {
            CLOSED_SEEN.set(true);
            return Pull::Closed;
        };
        let deadline = match wait {
            Wait::For(t) => Some(Instant::now() + t),
            Wait::Forever | Wait::Never => None,
        };
        loop {
            // The app instance took its last unit; the worker stays for the next one.
            if quota::exhausted() {
                return Pull::Closed;
            }
            sb_update(Event::Idle);
            let got = match wait {
                Wait::Never => unit_r.rx.try_recv().map_err(|e| match e {
                    TryRecvError::Empty => RecvTimeoutError::Timeout,
                    TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                }),
                Wait::Forever => unit_r.recv(None),
                Wait::For(_) => {
                    unit_r.recv(deadline.map(|d| d.saturating_duration_since(Instant::now())))
                }
            };
            sb_update(Event::Active);
            match got {
                Ok(Entry::Unit { unit, .. }) => {
                    let unit = unit_r.take(unit);
                    if unit.cancelled() {
                        // Counted like a shed unit: the app never served it, so the max_requests quota does not tick.
                        sb_update(Event::Shed);
                        continue;
                    }
                    RECEIVED.set(true);
                    return Pull::Unit(unit);
                }
                Ok(Entry::RestartApp) if unit_r.classic => {
                    debug!(target: "rapira", "reload ignored: classic mode runs every unit in a new cycle");
                }
                // The current app instance takes no more units; the next one takes the rest.
                Ok(Entry::RestartApp) => {
                    info!(target: "rapira", "reload requested; restarting the app");
                    quota::exhaust();
                }
                Err(RecvTimeoutError::Timeout) => return Pull::NoUnit,
                Err(RecvTimeoutError::Disconnected) => {
                    CLOSED_SEEN.set(true);
                    return Pull::Closed;
                }
            }
        }
    })
}

/// The host pulls a unit to shed it after a failed boot cycle, and waits at most `timeout`. Before this pull, the worker stored starting, or draining after the drain decision. The pull keeps the slot state and the quota.
pub(crate) fn pull_unit_to_shed(timeout: Duration) -> Pull {
    UNIT_RX.with_borrow(|slot| {
        let Some(unit_r) = slot.as_ref() else {
            return Pull::Closed;
        };
        match unit_r.recv(Some(timeout)) {
            Ok(Entry::Unit { unit, .. }) => Pull::Unit(unit_r.take(unit)),
            // A reload during a failed boot: the next boot starts at once.
            Ok(Entry::RestartApp) | Err(RecvTimeoutError::Timeout) => Pull::NoUnit,
            Err(RecvTimeoutError::Disconnected) => Pull::Closed,
        }
    })
}

pub(crate) fn pending_depth() -> u64 {
    UNIT_RX.with_borrow(|slot| {
        slot.as_ref()
            .map_or(0, |unit_r| unit_r.pending.load(Ordering::Relaxed))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Instant;

    use super::*;
    use crate::scoreboard::sb_set;
    use crate::work::Held;

    /// A unit on the intake. `cancelled`: its client left.
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
    }

    /// One worker on this thread: a slot on a board in memory, its quota and its intake.
    struct TestWorker {
        slot: &'static rapira_scoreboard::SharedSlot,
        intake: Intake,
    }

    fn worker(max_requests: u64, classic: bool) -> TestWorker {
        let slot = &rapira_scoreboard::create(1).unwrap()[0];
        sb_set(slot);
        quota::install(max_requests, Stopper::default());
        RECEIVED.set(false);
        CLOSED_SEEN.set(false);
        let (tx, rx) = channel::<Entry>();
        UNIT_RX.with_borrow_mut(|unit_rx| {
            *unit_rx = Some(UnitRx {
                rx,
                pending: &slot.pending,
                classic,
            })
        });
        TestWorker {
            slot,
            intake: Intake::new(tx, slot),
        }
    }

    fn kind(pull: &Pull) -> &'static str {
        match pull {
            Pull::Unit(_) => "unit",
            Pull::Closed => "closed",
            Pull::NoUnit => "no unit",
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
            /// The app served one unit: the quota ticks.
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
            classic: bool,
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
                classic: false,
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
                classic: false,
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
                classic: true,
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
                classic: true,
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
                classic: false,
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
                classic: false,
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
                name: "a reload closes the app instance and leaves the unit to the next one",
                max_requests: 10,
                classic: false,
                feed: &[Feed::Reload, Feed::Unit],
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
                name: "classic mode ignores a reload",
                max_requests: 0,
                classic: true,
                feed: &[Feed::Reload, Feed::Unit],
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
                name: "an exhausted quota pulls nothing",
                max_requests: 1,
                classic: false,
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
                name: "a closed intake",
                max_requests: 0,
                classic: false,
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
                classic: false,
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
                classic: false,
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
            let TestWorker { slot, intake } = worker(case.max_requests, case.classic);
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
                    Feed::Reload => intake.as_ref().unwrap().restart_app().unwrap(),
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
                    slot.handled.load(Relaxed),
                    slot.errors.load(Relaxed),
                    slot.pending.load(Relaxed),
                ),
                (case.counts.handled, case.counts.errors, case.counts.pending),
                "{}: handled, errors, pending",
                case.name
            );
        }
    }
}
