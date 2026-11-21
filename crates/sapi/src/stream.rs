//! The stream layer that every plugin shares. It parks the PHP thread until the host has an event. The generator loop is in rapira_stream.c.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Handle;

pub use crate::start::Wait;

/// The runtime of the plugin of this worker. `run_plugin` sets it before the plugin serves.
pub(crate) static HANDLE: OnceLock<Handle> = OnceLock::new();

/// The timer of zend_set_timeout on NTS: ITIMER_REAL on Apple Silicon, ITIMER_PROF (process CPU time) elsewhere. https://github.com/php/php-src/blob/php-8.5.12/Zend/zend_execute_API.c#L1647-L1655
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ZEND_TIMER: libc::c_int = libc::ITIMER_REAL;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const ZEND_TIMER: libc::c_int = libc::ITIMER_PROF;

/// Runs `park` with the execution timer paused, then restores the time that the timer had left. A parked thread never reaches an opcode boundary, and on NTS a second expiry would `_exit(124)`. Call it on the PHP thread, because the timer belongs to the process.
pub fn paused<R>(park: impl FnOnce() -> R) -> R {
    // SAFETY: the all-zero itimerval is a valid value. It is a timer that is off.
    let off: libc::itimerval = unsafe { std::mem::zeroed() };
    let mut left = off;
    // SAFETY: both pointers are live itimerval values. https://man7.org/linux/man-pages/man2/setitimer.2.html
    unsafe { libc::setitimer(ZEND_TIMER, &off, &mut left) };
    let result = park();
    // A zero value is a timer that was off or that expired. An expired timer has already set EG(timed_out).
    if left.it_value.tv_sec != 0 || left.it_value.tv_usec != 0 {
        #[cfg(target_os = "linux")]
        less_one_tick(&mut left.it_value);
        // SAFETY: left is a live itimerval, and a null old value is allowed.
        unsafe { libc::setitimer(ZEND_TIMER, &left, std::ptr::null_mut()) };
    }
    result
}

/// Takes one tick from the time that the timer had left. Linux adds one tick to an ITIMER_PROF value at each arming, so each park would give the app one tick more. https://github.com/torvalds/linux/blob/v7.3/kernel/time/itimer.c#L201-L205 The resolution of a coarse clock is the same tick. https://github.com/torvalds/linux/blob/v7.3/include/vdso/ktime.h#L13
#[cfg(target_os = "linux")]
fn less_one_tick(left: &mut libc::timeval) {
    // SAFETY: the all-zero timespec is a valid value.
    let mut tick: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: tick is a live timespec. https://man7.org/linux/man-pages/man2/clock_getres.2.html
    unsafe { libc::clock_getres(libc::CLOCK_REALTIME_COARSE, &mut tick) };
    // A value of zero turns the timer off, so a timer with one tick or less left keeps 1 us.
    let usec = (left.tv_sec * 1_000_000 + left.tv_usec - tick.tv_nsec / 1000).max(1);
    left.tv_sec = usec / 1_000_000;
    left.tv_usec = usec % 1_000_000;
}

/// What ended a park.
#[derive(Debug, PartialEq)]
pub enum Step<T> {
    Item(T),
    /// The source of the items ended.
    End,
    /// The host closed the unit.
    Closed,
    /// No item arrives within the wait.
    Timeout,
}

/// The first of three events, checked in this order: `closed` completes, `next` completes, the wait ends. `Wait::Never` polls each future once.
async fn first<T>(
    next: impl Future<Output = Option<T>>,
    closed: impl Future<Output = ()>,
    wait: Wait,
) -> Step<T> {
    let time = async {
        match wait {
            Wait::Forever => std::future::pending().await,
            Wait::For(d) => tokio::time::sleep(d).await,
            Wait::Never => {}
        }
    };
    tokio::select! {
        biased;
        () = closed => Step::Closed,
        item = next => item.map_or(Step::End, Step::Item),
        () = time => Step::Timeout,
    }
}

/// Blocks the PHP thread until `closed` completes, `next` completes or the wait ends, in this order of precedence. The PHP thread drives only these futures on the plugin runtime; the IO threads of the runtime handle the sockets and the timers.
pub fn wait<T>(
    next: impl Future<Output = Option<T>>,
    closed: impl Future<Output = ()>,
    wait: Wait,
) -> Step<T> {
    let handle = HANDLE
        .get()
        .expect("run_plugin sets the handle before the plugin submits a unit");
    paused(|| handle.block_on(first(next, closed, wait)))
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::time::Duration;

    use super::*;

    /// The decided order of a park gives the expected values: the close first, then the item, then the time. A wait of zero polls each future once.
    #[tokio::test(start_paused = true)]
    async fn the_first_event_of_a_park_follows_the_order_close_item_time() {
        struct Case {
            name: &'static str,
            closed: bool,
            /// None means that no item is ready.
            next: Option<Option<u8>>,
            wait: Wait,
            expected: Step<u8>,
        }
        let cases = [
            Case {
                name: "a close wins over a ready item",
                closed: true,
                next: Some(Some(7)),
                wait: Wait::Forever,
                expected: Step::Closed,
            },
            Case {
                name: "an item wins over a wait of zero",
                closed: false,
                next: Some(Some(7)),
                wait: Wait::Never,
                expected: Step::Item(7),
            },
            Case {
                name: "the source ended",
                closed: false,
                next: Some(None),
                wait: Wait::Forever,
                expected: Step::End,
            },
            Case {
                name: "nothing ready and no wait",
                closed: false,
                next: None,
                wait: Wait::Never,
                expected: Step::Timeout,
            },
            Case {
                name: "nothing ready within the wait",
                closed: false,
                next: None,
                wait: Wait::For(Duration::from_secs(1)),
                expected: Step::Timeout,
            },
        ];
        for case in cases {
            let next = async {
                match case.next {
                    Some(item) => item,
                    None => pending().await,
                }
            };
            let closed = async {
                if !case.closed {
                    pending::<()>().await;
                }
            };
            let got = first(next, closed, case.wait).await;
            assert_eq!(got, case.expected, "{}", case.name);
        }
    }
}
