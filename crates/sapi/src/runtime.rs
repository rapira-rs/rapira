use crate::RapiraHandle;
use crate::api::{Extension, Php, PrepareCtx};
use http::header::CONTENT_TYPE;
use std::future::Future;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::multipart;

type Outcome = std::result::Result<(), String>;
type BoxFuture = Pin<Box<dyn Future<Output = Outcome> + Send>>;

/// Object-safe shim: the same extension value is prepared pre-fork and launched post-fork, so it crosses the fork.
trait ErasedExt {
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> anyhow::Result<()>;
    fn launch(self: Box<Self>, php: Php, stop: watch::Receiver<bool>, grace: Duration)
    -> BoxFuture;
}

impl<E: Extension> ErasedExt for E {
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> anyhow::Result<()> {
        Extension::prepare(self, ctx)
    }

    fn launch(
        self: Box<Self>,
        php: Php,
        stop: watch::Receiver<bool>,
        grace: Duration,
    ) -> BoxFuture {
        Box::pin(drive(*self, php, stop, grace))
    }
}

struct Registered {
    name: String,
    ext: Box<dyn ErasedExt>,
}

#[derive(Default)]
pub struct ExtensionRuntime {
    exts: Vec<Registered>,
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<E: Extension>(&mut self, config: E::Config) {
        let ext = E::init(config);
        self.exts.push(Registered {
            name: ext.name().to_string(),
            ext: Box::new(ext),
        });
    }

    /// Master-side, pre-fork: runs every extension's `prepare` in registration order.
    pub fn prepare_all(&mut self, ctx: &mut PrepareCtx) -> anyhow::Result<()> {
        use anyhow::Context;
        for Registered { name, ext } in &mut self.exts {
            ext.prepare(ctx)
                .with_context(|| format!("extension {name}: prepare failed"))?;
        }
        Ok(())
    }

    pub fn run(self, rapira: RapiraHandle, script: PathBuf) -> Running {
        self.run_with_options(rapira, script, RuntimeOptions::default())
    }

    /// One worker thread: this runtime drives only the `drive` future of each extension, and it exists in every forked worker process.
    pub fn run_with_options(
        self,
        rapira: RapiraHandle,
        script: PathBuf,
        opts: RuntimeOptions,
    ) -> Running {
        let grace = opts.grace;
        let php = Php::new(Arc::new(RapiraBackend::new(rapira, &script, opts)));
        let (stop_tx, stop_rx) = watch::channel(false);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .thread_name("rapira-ext")
            .build()
            .expect("build extension runtime");

        let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();
        for Registered { name, ext } in self.exts {
            let (php, stop) = (php.clone(), stop_rx.clone());
            let fut = ext.launch(php, stop, grace);
            tasks.spawn_on(
                async move {
                    let outcome = fut.await;
                    match &outcome {
                        Ok(()) => tracing::info!(target: "ext", "{name} finished"),
                        Err(msg) => tracing::error!(target: "ext", "{name}: {msg}"),
                    }
                    outcome
                },
                rt.handle(),
            );
        }

        Running { rt, tasks, stop_tx }
    }
}

pub struct RuntimeOptions {
    pub grace: Duration,
    /// Host-parsed multipart limits: read only on a dispatcher handle, the worker arm feeds php-src's own rfc1867 through read_post.
    pub uploads: Arc<multipart::Limits>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            uploads: Arc::new(multipart::Limits::default()),
        }
    }
}

struct RapiraBackend {
    rapira: RapiraHandle,
    uploads: Arc<multipart::Limits>,
}

/// A refusal before dispatch: PHP never saw the work.
fn refused(e: crate::HandleError) -> anyhow::Error {
    anyhow::Error::new(crate::api::Rejected {
        status: match e {
            crate::HandleError::Saturated => 503,
            crate::HandleError::Stopped => 500,
        },
        reason: e.to_string(),
    })
}

fn parse_err(e: multipart::ParseError) -> anyhow::Error {
    match e {
        multipart::ParseError::Rejected(r) => anyhow::Error::new(r),
        multipart::ParseError::Io(io) => anyhow::Error::new(io).context("upload spool failed"),
    }
}

impl RapiraBackend {
    fn new(rapira: RapiraHandle, filename: &Path, opts: RuntimeOptions) -> Self {
        crate::set_script(filename);
        Self {
            rapira,
            uploads: opts.uploads,
        }
    }

    /// Multipart parses here, pre-enqueue: a rejected body never reaches the pending/active counters.
    async fn to_request(&self, mut req: crate::api::Request) -> anyhow::Result<crate::Request> {
        let content_type = req.headers.get(CONTENT_TYPE).map(|v| v.as_bytes().to_vec());
        let content_length = req.body.len() as i64;

        // Content-type is a singleton field per RFC 9110 §8.3: with repeated lines the host and a PHP consumer could split the body on different boundaries.
        // https://www.rfc-editor.org/rfc/rfc9110#section-8.3
        if self.rapira.dispatcher() && !req.body.is_empty() {
            let lines = req.headers.get_all(CONTENT_TYPE);
            if lines.iter().nth(1).is_some()
                && lines.iter().any(|v| multipart::is_multipart(v.as_bytes()))
            {
                return Err(anyhow::Error::new(crate::api::Rejected {
                    status: 400,
                    reason: "repeated content-type field lines with a multipart body".into(),
                }));
            }
        }

        let body = if self.rapira.dispatcher()
            && !req.body.is_empty()
            && let Some(ct) = content_type.as_deref()
            && multipart::is_multipart(ct)
        {
            let boundary = multipart::boundary(ct).map_err(parse_err)?;
            let bytes = std::mem::take(&mut req.body);
            let limits = Arc::clone(&self.uploads);
            let parsed =
                tokio::task::spawn_blocking(move || multipart::parse(&bytes, &boundary, &limits))
                    .await
                    .map_err(|e| anyhow::anyhow!("multipart parse task failed: {e}"))?;
            crate::types::Body::Multipart(parsed.map_err(parse_err)?)
        } else {
            crate::types::Body::Raw(Cursor::new(std::mem::take(&mut req.body)))
        };

        Ok(crate::Request {
            method: req.method,
            https: req.https,
            protocol: req.protocol,
            target: req.target,
            authority: req.authority,
            remote: req.remote,
            server: req.server,
            server_name: req.server_name,
            server_port: req.server_port,
            content_type,
            content_length,
            body,
            headers: req.headers,
            uri: req.uri,
            received_at: req.received_at,
            tls: req.tls,
        })
    }
}

impl crate::api::Backend for RapiraBackend {
    /// The Reply wraps the frame receiver directly, so dropping it is the client-gone signal the exchange layer observes.
    fn exec(
        &self,
        req: crate::api::Request,
    ) -> Pin<Box<dyn Future<Output = crate::api::Result<crate::api::Reply>> + Send + '_>> {
        Box::pin(async move {
            let req = self.to_request(req).await?;
            let rx = self.rapira.handle(req).await.map_err(refused)?;
            Ok(crate::api::Reply::new(rx))
        })
    }

    fn unary(
        &self,
        call: crate::api::UnaryCall,
    ) -> Pin<Box<dyn Future<Output = crate::api::Result<Option<crate::api::UnaryReply>>> + Send + '_>>
    {
        Box::pin(async move {
            let rx = self.rapira.call(call).await.map_err(refused)?;
            // A closed channel means that PHP lost the call.
            Ok(rx.await.ok())
        })
    }
}

/// On stop the `run` future is dropped first, releasing `&mut ext` so `shutdown` can drain within `grace`.
async fn drive<E: Extension>(
    mut ext: E,
    php: Php,
    mut stop: watch::Receiver<bool>,
    grace: Duration,
) -> Outcome {
    let finished = {
        let run = ext.run(php);
        tokio::pin!(run);
        tokio::select! {
            outcome = &mut run => Some(outcome),
            _ = stop.wait_for(|stopping| *stopping) => None,
        }
    };
    match finished {
        Some(outcome) => outcome.map_err(|e| format!("run failed: {e:#}")),
        None => match tokio::time::timeout(grace, ext.shutdown()).await {
            Ok(result) => result.map_err(|e| format!("shutdown failed: {e:#}")),
            Err(_) => Err("shutdown timed out".into()),
        },
    }
}

fn sigset(signals: &[libc::c_int]) -> libc::sigset_t {
    // SAFETY: operates on a stack-owned, freshly-initialized signal set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &sig in signals {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// Blocks until one of `signals` (already blocked) is delivered. https://man7.org/linux/man-pages/man3/sigwait.3.html
fn wait_signal(signals: &[libc::c_int]) -> libc::c_int {
    // SAFETY: `set` and `sig` are stack values live for the whole call.
    unsafe {
        let set = sigset(signals);
        let mut sig: libc::c_int = 0;
        libc::sigwait(&set, &mut sig);
        sig
    }
}

/// Graceful-stop handle callable from plain threads: `watch::Sender::send` needs no runtime.
#[derive(Clone)]
pub struct Stopper(watch::Sender<bool>);

impl Stopper {
    pub fn stop(&self) {
        let _ = self.0.send(true);
    }
}

pub struct Running {
    rt: Runtime,
    tasks: JoinSet<Outcome>,
    stop_tx: watch::Sender<bool>,
}

impl Running {
    pub fn join(mut self) -> Vec<Outcome> {
        self.drain_all()
    }

    pub fn stop(self) -> Vec<Outcome> {
        let _ = self.stop_tx.send(true);
        self.join()
    }

    pub fn stopper(&self) -> Stopper {
        Stopper(self.stop_tx.clone())
    }

    /// Forked-worker entry: requires the fork bracket to have masked exactly {QUIT, INT} in the child; the first signal drains, a second force-exits 131.
    pub fn serve_worker(mut self) -> Vec<Outcome> {
        let stop_tx = self.stop_tx.clone();
        std::thread::Builder::new()
            .name("rapira-worker-signal".into())
            .spawn(move || {
                let sig = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
                tracing::info!(target: "rapira", "signal {sig} received; draining worker");
                let _ = stop_tx.send(true);
                let _ = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
                tracing::warn!(target: "rapira", "second signal; forcing worker exit");
                std::process::exit(131);
            })
            .expect("spawn worker signal thread");
        self.drain_all()
    }

    fn drain_all(&mut self) -> Vec<Outcome> {
        let mut tasks = std::mem::take(&mut self.tasks);
        self.rt.block_on(drain(&mut tasks))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
        let _ = self.drain_all();
    }
}

async fn drain(tasks: &mut JoinSet<Outcome>) -> Vec<Outcome> {
    let mut out = Vec::with_capacity(tasks.len());
    while let Some(joined) = tasks.join_next().await {
        out.push(joined.unwrap_or_else(|_| Err("driver task panicked".into())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_err` keeps both causes typed: `io::Error` in the chain, `Rejected` downcastable.
    #[test]
    fn parse_err_keeps_the_typed_causes() {
        let io = parse_err(multipart::ParseError::Io(std::io::Error::other(
            "disk full",
        )));
        assert!(io.chain().any(|c| c.is::<std::io::Error>()));
        assert!(io.downcast_ref::<crate::api::Rejected>().is_none());

        let rejected = parse_err(multipart::ParseError::Rejected(crate::api::Rejected {
            status: 413,
            reason: "too big".into(),
        }));
        assert_eq!(
            rejected
                .downcast_ref::<crate::api::Rejected>()
                .map(|r| r.status),
            Some(413)
        );
    }

    /// `sigwait` dequeues a blocked, pending signal instead of running the default terminate action.
    #[test]
    fn sigwait_reaps_a_blocked_signal() {
        let set = sigset(&[libc::SIGTERM]);
        // SAFETY: SIGTERM is blocked in this thread, so `raise` leaves it pending for `sigwait` to dequeue.
        unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::raise(libc::SIGTERM);
        }
        assert_eq!(wait_signal(&[libc::SIGTERM]), libc::SIGTERM);
    }
}
