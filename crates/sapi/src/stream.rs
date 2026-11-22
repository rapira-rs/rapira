//! The stream layer that every plugin shares. It parks the PHP thread until the host has an event. The generator loop is in rapira_stream.c.

use std::future::Future;
use std::mem::MaybeUninit;
use std::sync::OnceLock;
use std::time::Instant;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

pub use crate::start::Wait;

/// The shared IO runtime. The host sets the handle before any plugin serves.
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
        unsafe { crate::watchdog::resume_timer(saved.assume_init_ref()) };
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

/// An interrupt or a close takes priority over a ready item. A ready item takes priority over the end of the wait.
async fn first<T>(
    next: impl Future<Output = Option<T>>,
    closed: impl Future<Output = ()>,
    wait: Wait,
    mut interrupt: Option<watch::Receiver<bool>>,
) -> Step<T> {
    let time = async {
        match wait {
            Wait::Forever => std::future::pending().await,
            Wait::For(d) => tokio::time::sleep(d).await,
            Wait::Never => {}
        }
    };
    let wake = async {
        match &mut interrupt {
            Some(rx) => {
                let _ = rx.wait_for(|&fired| fired).await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = wake => Step::Closed,
        () = closed => Step::Closed,
        item = next => item.map_or(Step::End, Step::Item),
        () = time => Step::Timeout,
    }
}

/// Blocks the PHP thread until an interrupt, a close, an item or the end of the wait. The IO threads handle the sockets and timers.
/// # Safety
/// The caller runs on a PHP thread.
pub unsafe fn wait<T>(
    next: impl Future<Output = Option<T>>,
    closed: impl Future<Output = ()>,
    wait: Wait,
) -> Step<T> {
    let handle = HANDLE
        .get()
        .expect("the host sets the IO runtime before a plugin submits a unit");
    // SAFETY: the caller runs on a PHP thread.
    unsafe { paused(|| handle.block_on(first(next, closed, wait, crate::watchdog::wake()))) }
}

/// Sends a response frame. A full channel parks the worker until space is available, the receiver closes, the watchdog fires, or the stream deadline expires.
/// # Safety
/// The calling thread runs an active PHP request.
pub unsafe fn send<T>(tx: &mpsc::Sender<T>, item: T, until: Option<Instant>) -> bool {
    let item = match tx.try_send(item) {
        Ok(()) => return true,
        Err(mpsc::error::TrySendError::Closed(_)) => return false,
        Err(mpsc::error::TrySendError::Full(item)) => item,
    };
    let limit = until.map_or(Wait::Forever, |t| {
        Wait::For(t.saturating_duration_since(Instant::now()))
    });
    let write = async { tx.send(item).await.ok() };
    matches!(unsafe { wait(write, tx.closed(), limit) }, Step::Item(()))
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
            let got = first(next, closed, case.wait, None).await;
            assert_eq!(got, case.expected, "{}", case.name);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_releases_a_parked_response_write_until_the_next_unit() {
        use tokio::sync::{mpsc, watch};

        let (tx, mut rx) = mpsc::channel(1);
        tx.send(1).await.unwrap();
        let wake = watch::Sender::new(false);
        let write = || {
            first(
                async { tx.send(2).await.ok() },
                tx.closed(),
                Wait::Forever,
                Some(wake.subscribe()),
            )
        };
        let mut parked = std::pin::pin!(write());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut parked)
                .await
                .is_err()
        );
        wake.send_replace(true);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), parked)
                .await
                .expect("the interrupt wakes the writer"),
            Step::Closed
        );
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(write().await, Step::Closed, "a later park of the same unit");
        wake.send_replace(false);
        assert_eq!(write().await, Step::Item(()), "the next unit can write");
        assert_eq!(rx.recv().await, Some(2));
    }
}
