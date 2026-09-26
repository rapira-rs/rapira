use std::ffi::CStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crate::scoreboard::{Event, sb_update};
use crate::{zend_class_entry, zend_object};

pub(crate) const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// The cycle bookkeeping view of a unit that receive() handed out.
pub trait Held {
    /// The worker committed the outcome, or discarded the unit.
    fn finalized(&self) -> bool;
    /// The host no longer takes an outcome: Work::isCancelled().
    fn host_closed(&self) -> bool;
    fn discard(&mut self);

    /// Work::isFinalized().
    fn is_finalized(&self) -> bool {
        self.finalized() || self.host_closed()
    }
}

/// Reclaims the Box that receive() handed out: clears the cycle slot if it still points here, and counts an unfinalized unit as handled.
/// # Safety
/// `ptr` came from `Box::into_raw` in receive and was not reclaimed before.
pub(crate) unsafe fn release<T: Held + ?Sized>(ptr: *mut T) -> Box<T> {
    crate::exchange::forget_held(ptr.cast::<()>());
    let st = unsafe { Box::from_raw(ptr) };
    if !st.finalized() {
        sb_update(Event::Handled(true));
    }
    st
}

/// One unit of work on the intake. The PHP thread sees only this trait.
pub trait Work: Send + 'static {
    /// The client left while the unit was queued: receive() skips it.
    fn cancelled(&self) -> bool;
    /// Dispatcher mode: attaches the unit to the object receive() allocated from `DispatcherClasses::unit`.
    /// # Safety
    /// `obj` is a live object of that class on the PHP thread.
    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held;
    /// The classic and worker modes. None: this unit cannot run there.
    fn into_cgi(self: Box<Self>) -> Option<crate::types::Context>;
    /// A worker that cannot serve: the plugin's refusal, 503 or UNAVAILABLE.
    fn shed(self: Box<Self>);
}

/// The class entries of one plugin's dispatcher surface. Set once per worker.
#[derive(Clone, Copy)]
pub struct DispatcherClasses {
    pub dispatcher: unsafe fn() -> *mut zend_class_entry,
    pub info: unsafe fn() -> *mut zend_class_entry,
    pub unit: unsafe fn() -> *mut zend_class_entry,
    /// The receive() error while a unit is unfinalized.
    pub busy: &'static CStr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Saturated,
    Stopped,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saturated => write!(f, "worker pool saturated for {INTAKE_WAIT:?}"),
            Self::Stopped => write!(f, "worker pool stopped"),
        }
    }
}

impl std::error::Error for Refused {}

pub(crate) fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The queue from a plugin thread to the PHP thread. Clone per plugin task.
#[derive(Clone)]
pub struct Sink {
    tx: SyncSender<Box<dyn Work>>,
    pending: Arc<AtomicUsize>,
}

struct PendingGuard<'a>(Option<&'a AtomicUsize>);

impl<'a> PendingGuard<'a> {
    fn arm(pending: &'a AtomicUsize) -> Self {
        pending.fetch_add(1, Ordering::Relaxed);
        Self(Some(pending))
    }
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Some(pending) = self.0.take() {
            pending.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Sink {
    pub(crate) fn new(tx: SyncSender<Box<dyn Work>>, pending: Arc<AtomicUsize>) -> Self {
        Self { tx, pending }
    }

    /// pending is incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero.
    pub async fn submit(&self, mut unit: Box<dyn Work>) -> Result<(), Refused> {
        let pending = PendingGuard::arm(&self.pending);
        let deadline = Instant::now() + INTAKE_WAIT;
        loop {
            match self.tx.try_send(unit) {
                Ok(()) => {
                    pending.disarm();
                    return Ok(());
                }
                Err(TrySendError::Full(u)) => {
                    if Instant::now() > deadline {
                        tracing::warn!(
                            target: "rapira",
                            "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                            self.pending.load(Ordering::Relaxed)
                        );
                        return Err(Refused::Saturated);
                    }
                    unit = u;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(Refused::Stopped),
            }
        }
    }

    /// A sink with no PHP thread. The receiver plays the PHP thread.
    pub fn channel(cap: usize) -> (Sink, std::sync::mpsc::Receiver<Box<dyn Work>>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(cap);
        (Self::new(tx, Arc::new(AtomicUsize::new(0))), rx)
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
}

/// The typed handle a plugin's transport submits to.
pub struct Intake<U: Work> {
    route: Route<U>,
}

enum Route<U> {
    Sink(Sink),
    Channel(tokio::sync::mpsc::Sender<U>),
}

impl<U: Work> Clone for Intake<U> {
    fn clone(&self) -> Self {
        let route = match &self.route {
            Route::Sink(sink) => Route::Sink(sink.clone()),
            Route::Channel(tx) => Route::Channel(tx.clone()),
        };
        Self { route }
    }
}

impl<U: Work> Intake<U> {
    /// The worker behind `sink` must have started with the `DispatcherClasses` of the plugin that owns `U`.
    pub fn new(sink: Sink) -> Self {
        Self {
            route: Route::Sink(sink),
        }
    }

    /// A test intake with no PHP thread. The receiver plays the PHP thread.
    pub fn channel(cap: usize) -> (Self, tokio::sync::mpsc::Receiver<U>) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        let intake = Self {
            route: Route::Channel(tx),
        };
        (intake, rx)
    }

    pub async fn submit(&self, unit: U) -> Result<(), Refused> {
        match &self.route {
            Route::Sink(sink) => sink.submit(Box::new(unit)).await,
            Route::Channel(tx) => match tokio::time::timeout(INTAKE_WAIT, tx.send(unit)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(Refused::Stopped),
                Err(_) => Err(Refused::Saturated),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;
    impl Work for Probe {
        fn cancelled(&self) -> bool {
            false
        }
        unsafe fn attach(self: Box<Self>, _: *mut zend_object) -> *mut dyn Held {
            unreachable!()
        }
        fn into_cgi(self: Box<Self>) -> Option<crate::types::Context> {
            None
        }
        fn shed(self: Box<Self>) {}
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_intake_delivers_in_order() {
        let (intake, mut rx) = Intake::<Probe>::channel(2);
        intake.submit(Probe).await.unwrap();
        intake.submit(Probe).await.unwrap();
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_intake_reports_stopped_after_the_receiver_is_gone() {
        let (intake, rx) = Intake::<Probe>::channel(1);
        drop(rx);
        assert_eq!(intake.submit(Probe).await.unwrap_err(), Refused::Stopped);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn channel_intake_reports_saturated_when_nothing_drains() {
        let (intake, _rx) = Intake::<Probe>::channel(1);
        intake.submit(Probe).await.unwrap();
        let second = tokio::spawn(async move { intake.submit(Probe).await });
        tokio::task::yield_now().await;
        tokio::time::advance(INTAKE_WAIT + std::time::Duration::from_secs(1)).await;
        assert_eq!(second.await.unwrap().unwrap_err(), Refused::Saturated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sink_counts_pending_until_the_consumer_pulls() {
        let (sink, _rx) = Sink::channel(1);
        sink.submit(Box::new(Probe)).await.unwrap();
        assert_eq!(sink.pending(), 1);
    }
}
