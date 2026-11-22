//! The stream layer that every plugin shares. It parks the PHP thread until the host has an event. The generator loop is in rapira_stream.c.

use std::future::Future;
use std::mem::MaybeUninit;
use std::sync::OnceLock;

use tokio::runtime::Handle;

pub use crate::start::Wait;

/// The runtime of the plugin of this worker. `run_plugin` sets it before the plugin serves.
pub(crate) static HANDLE: OnceLock<Handle> = OnceLock::new();

/// Runs `park` with the execution timer of this thread paused, then restores the timer. A parked thread never reaches an opcode boundary, and on NTS a second expiry would `_exit(124)`.
/// # Safety
/// The caller runs on a PHP thread.
pub unsafe fn paused<R>(park: impl FnOnce() -> R) -> R {
    let mut saved = MaybeUninit::<crate::rapira_timer>::uninit();
    // SAFETY: the caller runs on a PHP thread, and saved is writable.
    let live = unsafe { crate::rapira_timer_pause(saved.as_mut_ptr()) };
    let result = park();
    if live {
        // SAFETY: a true pause wrote saved.
        unsafe { crate::rapira_timer_resume(saved.as_ptr()) };
    }
    result
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
/// # Safety
/// The caller runs on a PHP thread.
pub unsafe fn wait<T>(
    next: impl Future<Output = Option<T>>,
    closed: impl Future<Output = ()>,
    wait: Wait,
) -> Step<T> {
    let handle = HANDLE
        .get()
        .expect("run_plugin sets the handle before the plugin submits a unit");
    // SAFETY: the caller runs on a PHP thread.
    unsafe { paused(|| handle.block_on(first(next, closed, wait))) }
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
