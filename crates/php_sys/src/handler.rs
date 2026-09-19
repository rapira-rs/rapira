use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::{
    start::Rapira,
    types::{Context, Frame, Job, Request},
    work::{PendingGuard, Queued, Work},
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
    intake: SyncSender<Queued>,
    pending: Arc<AtomicUsize>,
    dispatcher: bool,
    grpc: bool,
}

impl Rapira {
    pub fn handle(&self) -> RapiraHandle {
        let intake = self.intake.as_ref().expect("intake lives until Drop");
        RapiraHandle {
            intake: intake.tx.clone(),
            pending: intake.pending.clone(),
            dispatcher: self.dispatcher,
            grpc: self.grpc,
        }
    }
}

fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl RapiraHandle {
    pub fn dispatcher(&self) -> bool {
        self.dispatcher
    }

    // pending must be incremented before the send: the consumer decrements as soon as it wakes, so the reverse order could wrap the counter below zero
    pub async fn handle(&self, mut req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
        if self.grpc {
            return Err(HandleError::Stopped);
        }
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let job = Job {
            ctx: Context::new(req, tx, !self.dispatcher),
        };
        self.enqueue(Work::Http(Box::new(job))).await?;
        Ok(rx)
    }

    async fn enqueue(&self, work: Work) -> Result<(), HandleError> {
        let mut job = Queued {
            work,
            _pending: PendingGuard::arm(&self.pending),
        };
        let deadline = Instant::now() + INTAKE_WAIT;
        loop {
            match self.intake.try_send(job) {
                Ok(()) => {
                    return Ok(());
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
        if self.grpc {
            return Err(HandleError::Stopped);
        }
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        if self
            .intake
            .send(Queued {
                work: Work::Http(Box::new(Job {
                    ctx: Context::new(req, tx, !self.dispatcher),
                })),
                _pending: PendingGuard::arm(&self.pending),
            })
            .is_err()
        {
            return Err(HandleError::Stopped);
        }
        Ok(rx)
    }

    /// Dropping this future closes the per-call receiver, including while PHP is busy.
    pub async fn handle_grpc(
        &self,
        request: crate::grpc::Request,
    ) -> Result<crate::grpc::Reply, HandleError> {
        if !self.grpc {
            return Err(HandleError::Stopped);
        }
        let expires_at = request.expires_at;
        let wait = async {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.enqueue(Work::Grpc(Box::new(crate::grpc::Job { request, sender })))
                .await?;
            Ok(receiver
                .await
                .unwrap_or_else(|_| crate::grpc::Reply::error(13, "call failed")))
        };
        match expires_at {
            Some(end) if end <= Instant::now() => {
                Ok(crate::grpc::Reply::error(4, "deadline exceeded"))
            }
            Some(end) => {
                let result = tokio::time::timeout_at(end.into(), wait).await;
                match result {
                    Ok(reply) if Instant::now() < end => reply,
                    _ => Ok(crate::grpc::Reply::error(4, "deadline exceeded")),
                }
            }
            None => wait.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;

    #[test]
    fn elapsed_deadline_wins_over_a_ready_closed_reply() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let (intake, queue) = std::sync::mpsc::sync_channel(1);
                let handle = RapiraHandle {
                    intake,
                    pending: Arc::new(AtomicUsize::new(0)),
                    dispatcher: true,
                    grpc: true,
                };
                let request = crate::grpc::Request {
                    method: "example.Service/Call".into(),
                    message: bytes::Bytes::new(),
                    metadata: Vec::new(),
                    remote: crate::types::Addr::Unix(None),
                    tls: None,
                    received_at: 0.0,
                    deadline: None,
                    expires_at: Some(Instant::now() + Duration::from_millis(20)),
                };
                let mut pending = Box::pin(handle.handle_grpc(request));
                std::future::poll_fn(|cx| {
                    assert!(pending.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                let queued = queue.try_recv().unwrap();
                std::thread::sleep(Duration::from_millis(30));
                drop(queued);
                let reply = pending.await.unwrap();
                assert_eq!(reply.result.unwrap_err().code, 4);
                assert_eq!(handle.pending.load(Ordering::Relaxed), 0);
            });
    }
}
