//! Response writes from a PHP thread to the IO runtime.

use std::future::pending;
use std::mem::MaybeUninit;
use std::time::Instant;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

async fn send_parked<T>(
    tx: &mpsc::Sender<T>,
    item: T,
    until: Option<Instant>,
    mut interrupt: Option<watch::Receiver<bool>>,
) -> bool {
    let wake = async {
        match &mut interrupt {
            Some(rx) => {
                let _ = rx.wait_for(|&fired| fired).await;
            }
            None => pending().await,
        }
    };
    let deadline = async {
        match until {
            Some(t) => tokio::time::sleep_until(t.into()).await,
            None => pending().await,
        }
    };
    tokio::select! {
        biased;
        () = wake => false,
        result = tx.send(item) => result.is_ok(),
        () = deadline => false,
    }
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
    let mut saved = MaybeUninit::<crate::rapira_timer>::uninit();
    // SAFETY: the caller runs on a PHP thread. A successful pause initializes saved.
    let live = unsafe { crate::rapira_timer_pause(saved.as_mut_ptr()) };
    let sent = Handle::current().block_on(send_parked(tx, item, until, crate::watchdog::wake()));
    if live {
        unsafe { crate::watchdog::resume_timer(saved.assume_init_ref()) };
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn a_parked_write_checks_the_interrupt_then_space_then_time() {
        struct Case {
            name: &'static str,
            full: bool,
            interrupted: bool,
            closed: bool,
            accepted: bool,
        }
        let cases = [
            Case {
                name: "interrupt before space",
                full: false,
                interrupted: true,
                closed: false,
                accepted: false,
            },
            Case {
                name: "space before expiry",
                full: false,
                interrupted: false,
                closed: false,
                accepted: true,
            },
            Case {
                name: "expiry on a full channel",
                full: true,
                interrupted: false,
                closed: false,
                accepted: false,
            },
            Case {
                name: "receiver closed",
                full: false,
                interrupted: false,
                closed: true,
                accepted: false,
            },
        ];
        for case in cases {
            let (tx, mut rx) = mpsc::channel(1);
            if case.full {
                tx.try_send(1).unwrap();
            }
            if case.closed {
                rx.close();
            }
            let wake = watch::Sender::new(case.interrupted);
            let until = tokio::time::Instant::now().into_std();
            assert_eq!(
                send_parked(&tx, 2, Some(until), Some(wake.subscribe())).await,
                case.accepted,
                "{}",
                case.name
            );
            let expected = if case.full {
                Some(1)
            } else {
                case.accepted.then_some(2)
            };
            assert_eq!(rx.try_recv().ok(), expected, "{}", case.name);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_releases_a_parked_response_write_until_the_next_unit() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(1).await.unwrap();
        let wake = watch::Sender::new(false);
        let write = || send_parked(&tx, 2, None, Some(wake.subscribe()));
        let mut parked = std::pin::pin!(write());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut parked)
                .await
                .is_err()
        );
        wake.send_replace(true);
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), parked)
                .await
                .expect("the interrupt wakes the writer")
        );
        assert_eq!(rx.recv().await, Some(1));
        assert!(!write().await, "a later park of the same unit");
        wake.send_replace(false);
        assert!(write().await, "the next unit can write");
        assert_eq!(rx.recv().await, Some(2));
    }
}
