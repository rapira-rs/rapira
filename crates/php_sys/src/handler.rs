use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::{
    start::Rapira,
    types::{Context, Frame, Job, Request},
};

// cap 4 lets a buffered Head+Chunk+End trio, plus a stray interim head, queue without parking the PHP thread
const FRAME_CAP: usize = 4;

const INTAKE_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    Saturated,
    Stopped,
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saturated => write!(f, "worker pool saturated for {INTAKE_WAIT:?}"),
            Self::Stopped => write!(f, "worker pool stopped"),
        }
    }
}

impl std::error::Error for HandleError {}

#[derive(Clone)]
pub struct RapiraHandle {
    intake: SyncSender<Box<Job>>,
    pending: Arc<AtomicUsize>,
    dispatcher: bool,
}

impl Rapira {
    pub fn handle(&self) -> RapiraHandle {
        let intake = self.intake.as_ref().expect("intake lives until Drop");
        RapiraHandle {
            intake: intake.tx.clone(),
            pending: intake.pending.clone(),
            dispatcher: self.dispatcher,
        }
    }
}

fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

impl RapiraHandle {
    pub fn dispatcher(&self) -> bool {
        self.dispatcher
    }

    // pending must be incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero
    pub async fn handle(&self, mut req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let mut job = Box::new(Job {
            ctx: Context::new(req, tx, !self.dispatcher),
        });
        let pending = PendingGuard::arm(&self.pending);
        let deadline = Instant::now() + INTAKE_WAIT;
        loop {
            match self.intake.try_send(job) {
                Ok(()) => {
                    pending.disarm();
                    return Ok(rx);
                }
                Err(TrySendError::Full(j)) => {
                    if Instant::now() > deadline {
                        tracing::warn!(
                            target: "rapira",
                            "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                            self.pending.load(Ordering::Relaxed)
                        );
                        return Err(HandleError::Saturated);
                    }
                    job = j;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(HandleError::Stopped),
            }
        }
    }

    pub fn handle_blocking(&self, mut req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let pending = PendingGuard::arm(&self.pending);
        if self
            .intake
            .send(Box::new(Job {
                ctx: Context::new(req, tx, !self.dispatcher),
            }))
            .is_err()
        {
            return Err(HandleError::Stopped);
        }
        pending.disarm();
        Ok(rx)
    }
}
