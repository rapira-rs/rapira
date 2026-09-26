use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::{
    start::Rapira,
    types::{Context, Frame, GrpcJob, GrpcOutcome, GrpcRequest, Job, Request, Unit},
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
    intake: SyncSender<Unit>,
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

    pub async fn handle(&self, mut req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let job = Box::new(Job {
            ctx: Context::new(req, tx, !self.dispatcher),
        });
        self.enqueue(Unit::Http(job)).await?;
        Ok(rx)
    }

    /// Queues one unary gRPC call. A dropped sender means that PHP lost the call; dropping the receiver closes the call for PHP.
    pub async fn call(
        &self,
        req: GrpcRequest,
    ) -> Result<oneshot::Receiver<GrpcOutcome>, HandleError> {
        let (reply, rx) = oneshot::channel();
        let job = Box::new(GrpcJob {
            req,
            received_at: now_unix_f64(),
            reply,
        });
        self.enqueue(Unit::Grpc(job)).await?;
        Ok(rx)
    }

    // pending must be incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero
    async fn enqueue(&self, mut unit: Unit) -> Result<(), HandleError> {
        let pending = PendingGuard::arm(&self.pending);
        let deadline = Instant::now() + INTAKE_WAIT;
        loop {
            match self.intake.try_send(unit) {
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
                        return Err(HandleError::Saturated);
                    }
                    unit = u;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(HandleError::Stopped),
            }
        }
    }
}
