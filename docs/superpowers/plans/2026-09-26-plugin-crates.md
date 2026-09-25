# Plugin crates implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the api, runtime and php_sys crates with one SAPI crate plus self-contained plugin crates, so that a plugin is one crate and every boundary type exists once.

**Architecture:** `rapira_sapi` keeps the embed SAPI, boot, the PHP thread, a bounded intake of `Box<dyn Work>`, the base PHP contract, the classic and worker modes, and a registry that plugins fill at MINIT and at `receive()`. Each plugin crate owns its config section, its PHP stub and C shells, the Rust behind them, its unit type and its transport. Both fronts run tower layers: `[http].middleware` and `[grpc].interceptors`.

**Tech Stack:** Rust 2024, tokio, hyper 1, tower, connectrpc, bindgen, cc, PHP embed SAPI (NTS).

**Spec:** `docs/superpowers/specs/2026-09-26-plugin-crates-design.md`

## Global Constraints

- NTS only, Unix only, one interpreter per forked worker, one plugin per pool and per worker process, master single-threaded without tokio, MINIT once in the master before the fork.
- Nothing PHP-visible changes. Every `*.stub.php` keeps its content. The PHP contract in `../contract` is not touched.
- The one word is `plugin`. No `extension`, `front` or `host` in new code, names, logs or docs.
- The SAPI crate is `rapira_sapi` in `crates/sapi`. The word `middleware` is HTTP only. The gRPC chain is `interceptors`.
- Every commit builds and passes `make test_nts`. Tasks 8 and 9 also pass `make test_e2e`.
- Commits are signed: `git commit -s -S`. Conventional Commits. No AI attribution trailers.
- Unit tests in-crate only when they need no fixture, no double and no socket. Everything else goes to `crates/tests`. No `testing.rs` and no harness dev-dependency in a plugin crate.
- Comments: STE, short, technical. No "instead of", no "previously". `.c` and `.h` files use `//` only.
- Do not add speculative code: no capability plugin, no interceptor implementation, no registry beyond the two hooks.
- The Windows fork is out of scope.

## Review Focus

- A `[grpc].interceptors` entry with any name must fail the boot with the full key in the message, because no interceptor ships. Test in Task 9.
- A pool mode the plugin does not serve, such as `[grpc.pool] mode = "worker"`, must fail the boot with the key and the allowed modes. Test in Task 4.
- A plugin's PHP classes must register after the base classes, because they extend `Rapira\Work` and friends. A boot with both plugin parts must expose every class. Test in Task 5.
- An intake built with `Intake::channel()` must report `Refused::Stopped` after the receiver is dropped, and `Refused::Saturated` when the receiver never drains and the wait expires. Test in Task 3.
- A unit whose client left before `receive()` must be skipped and counted as handled, for both unit types, so a queue full of dead requests never reaches PHP. Existing tests in `crates/tests/tests/dispatcher_loop.rs` and `grpc_dispatcher.rs` keep pinning this. Verify they still run in Task 4.

---

### Task 1: Rename php_sys to rapira_sapi and give the listener types to net

**Files:**
- Move: `crates/php_sys/` to `crates/sapi/`
- Move: `crates/api/src/prepare.rs` to `crates/net/src/listen.rs`
- Modify: `Cargo.toml` (workspace members and dependencies), every `Cargo.toml` that names `php_sys`, `crates/net/Cargo.toml`, `crates/net/src/lib.rs`, `crates/api/src/lib.rs`, `crates/api/Cargo.toml`
- Modify: every `use php_sys` and `php_sys::` path in `src/`, `crates/runtime`, `crates/tests`
- Modify: `Makefile` (the stub glob), `.github/workflows/ci.yml:191` (the `rapira_master` guard reads `cargo tree`, no change needed unless it names php_sys), `CONTRIBUTING.md:57`

**Interfaces:**
- Produces: crate `rapira_sapi` with the same public items as `php_sys` had. `rapira_net::{ListenAddr, PrepareCtx, PreparedListener}` with the same signatures as `extension_api::prepare` had. `extension_api` re-exports them from `rapira_net` so nothing else changes in this task.

- [ ] **Step 1: Move the crate directory and rename the package**

```bash
git mv crates/php_sys crates/sapi
sed -i 's/^name = "php_sys"/name = "rapira_sapi"/' crates/sapi/Cargo.toml
sed -i 's|"crates/php_sys"|"crates/sapi"|; s|^php_sys = { path = "crates/php_sys" }|rapira_sapi = { path = "crates/sapi" }|' Cargo.toml
grep -rl 'php_sys' --include=Cargo.toml . | grep -v target | xargs sed -i 's/^php_sys = { workspace = true }/rapira_sapi = { workspace = true }/'
grep -rl 'php_sys' --include=*.rs src crates | xargs sed -i 's/\bphp_sys\b/rapira_sapi/g'
sed -i 's|crates/php_sys/\*.stub.php|$$(find crates -name "*.stub.php" -not -path "*/target/*")|' Makefile
sed -i 's|`crates/php_sys`|`crates/sapi`|' CONTRIBUTING.md
```

The `build.rs` of the crate reads `CARGO_PKG_VERSION` and file names relative to the crate, so it needs no change. Check `crates/sapi/build.rs` for the string `php_sys` and `crates/tests/Cargo.toml` for a comment that names it.

- [ ] **Step 2: Move the listener types into net**

```bash
git mv crates/api/src/prepare.rs crates/net/src/listen.rs
```

In `crates/net/src/lib.rs` add `pub mod listen;` and `pub use listen::{ListenAddr, PrepareCtx, PreparedListener};`, and change the import `use extension_api::{ListenAddr, PreparedListener};` to `use listen::{ListenAddr, PreparedListener};`. Add `socket2 = "0.6"` and `anyhow` to `crates/net/Cargo.toml` `[dependencies]`, and `libc` to its `[dev-dependencies]` for the tests in `listen.rs`. Remove `extension_api` from `crates/net/Cargo.toml`.

In `crates/api/src/lib.rs` replace `mod prepare;` and `pub use prepare::{ListenAddr, PrepareCtx, PreparedListener};` with `pub use rapira_net::{ListenAddr, PrepareCtx, PreparedListener};`. Add `rapira_net = { workspace = true }` to `crates/api/Cargo.toml` and remove `socket2`. The tokio dev-dependency of `crates/api` stays for the middleware tests.

- [ ] **Step 3: Build and run the workspace tests**

Run: `cargo check --workspace --all-targets && make test_nts`
Expected: green. The four listener tests now run under `rapira_net`.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -s -S -m "refactor(sapi)!: rename php_sys to rapira_sapi and move the listener types to net"
```

---

### Task 2: Fold extension_api and rapira_runtime into rapira_sapi and delete the mirror types

**Files:**
- Move: `crates/api/src/lib.rs` to `crates/sapi/src/api.rs`, `crates/api/src/middleware.rs` to `crates/sapi/src/middleware.rs`
- Move: `crates/runtime/src/lib.rs` to `crates/sapi/src/runtime.rs`, `crates/runtime/src/multipart.rs` to `crates/sapi/src/multipart.rs`
- Delete: `crates/api/`, `crates/runtime/`
- Modify: `crates/sapi/Cargo.toml` (gains `http-body-util`, `socket2` is not needed, `tokio` features `rt-multi-thread`, `time`, `macros`, `sync`; `memchr`, `httparse`, `tempfile`; `rapira_net`), `crates/sapi/src/lib.rs` (`pub mod api; pub mod middleware; pub mod runtime; pub mod multipart;`)
- Modify: `crates/sapi/src/types.rs` (delete the mirrors), `crates/sapi/src/runtime.rs` (delete the mappers), every user of the deleted names
- Modify: `Cargo.toml`, `crates/plugins/http/Cargo.toml`, `crates/plugins/grpc/Cargo.toml`, `crates/middleware/static_files/Cargo.toml`, `crates/tests/Cargo.toml`, `src/main.rs`, `src/worker.rs`

**Interfaces:**
- Produces: `rapira_sapi::api::{Extension, Backend, Php, Request, Reply, ReplySource, Rejected, Addr, Tls, ClientCert, UnaryCall, UnaryReply, RpcStatus, RpcProtocol, Result}` as today, and `rapira_sapi::runtime::{ExtensionRuntime, RuntimeOptions, Running, Stopper}` as today. `rapira_sapi::types::{Addr, TlsView, ClientCertView, GrpcProtocol, GrpcRequest, GrpcOutcome, GrpcStatus}` are deleted; `rapira_sapi::types::Frame` stays and `api::ReplyEvent` is deleted.

- [ ] **Step 1: Move the files and delete the crates**

```bash
git mv crates/api/src/lib.rs crates/sapi/src/api.rs
git mv crates/api/src/middleware.rs crates/sapi/src/middleware.rs
git mv crates/runtime/src/lib.rs crates/sapi/src/runtime.rs
git mv crates/runtime/src/multipart.rs crates/sapi/src/multipart.rs
git rm -r crates/api crates/runtime
sed -i '/"crates\/runtime",/d; /"crates\/api",/d; /^rapira_runtime = /d; /^extension_api = /d' Cargo.toml
grep -rl 'extension_api\|rapira_runtime' --include=Cargo.toml . | grep -v target | xargs sed -i '/^extension_api = { workspace = true }/d; /^rapira_runtime = { workspace = true }/d'
```

Every crate that lost `extension_api` gets `rapira_sapi = { workspace = true }` if it does not have it. In `crates/sapi/src/api.rs` the line `mod middleware;` becomes `use crate::middleware;` and the `pub use middleware::{...}` stays. In `crates/sapi/src/runtime.rs` replace `use extension_api::` with `use crate::api::`, `php_sys::` and `rapira_sapi::` with `crate::`, and `pub mod multipart;` with `use crate::multipart;`. Add to `crates/sapi/src/lib.rs`:

```rust
pub mod api;
pub mod middleware;
pub mod multipart;
pub mod runtime;
```

Then replace the paths everywhere: `extension_api::` becomes `rapira_sapi::api::` and `rapira_runtime::` becomes `rapira_sapi::runtime::`.

```bash
grep -rl 'extension_api\|rapira_runtime' --include=*.rs src crates | xargs sed -i 's/\bextension_api::/rapira_sapi::api::/g; s/\brapira_runtime::/rapira_sapi::runtime::/g; s/^use extension_api;/use rapira_sapi::api;/'
```

- [ ] **Step 2: Build and run the workspace tests before deleting anything else**

Run: `cargo check --workspace --all-targets && make test_nts`
Expected: green. This is a pure move.

- [ ] **Step 3: Delete the mirror types and keep one of each**

Apply this table. The left name is deleted, the right name is used everywhere the left one was.

| Delete | Keep |
|---|---|
| `types::Addr` | `api::Addr` |
| `types::TlsView`, `types::ClientCertView` | `api::Tls`, `api::ClientCert` |
| `types::GrpcProtocol` | `api::RpcProtocol` |
| `types::GrpcRequest` | `api::UnaryCall` |
| `types::GrpcOutcome` (field `result`) | `api::UnaryReply` (field `outcome`) |
| `types::GrpcStatus` | `api::RpcStatus` |
| `api::ReplyEvent` | `types::Frame` |
| `runtime::map_addr`, `map_tls`, the protocol match, `FrameSource` | nothing: pass the value through |

`api::UnaryReply` gets `#[derive(Debug, PartialEq)]` and `api::UnaryCall` gets `#[derive(Debug)]`, because the sapi tests compare outcomes. `types::Request` keeps its shape; `runtime::RapiraBackend::to_request` keeps deriving `content_type`, `content_length` and `Body`, and stops mapping addresses and TLS. `api::Reply` wraps `tokio::sync::mpsc::Receiver<Frame>` directly:

```rust
pub struct Reply(tokio::sync::mpsc::Receiver<Frame>);

impl Reply {
    pub fn new(rx: tokio::sync::mpsc::Receiver<Frame>) -> Self {
        Self(rx)
    }

    pub fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Frame>> {
        self.0.poll_recv(cx)
    }

    pub async fn next(&mut self) -> Option<Frame> {
        self.0.recv().await
    }
}
```

The `ReplySource` trait is deleted. Every test double that implemented it (`TestSource` in `crates/plugins/http/src/handler.rs`, `Script` and `DrainSource` in `crates/plugins/http/src/bridge.rs`, `VecSource` in `crates/tests/src/lib.rs`) becomes a `tokio::sync::mpsc::channel::<Frame>(cap)` whose sender the test drives. A source that parked until released now holds the sender and sends on release. A source that ended without `End` drops the sender. The `Frame::Interim(ResponseHead)` and `Frame::Head { head, .. }` shapes stay; the fronts read `head.status` and `head.headers`.

- [ ] **Step 4: Build, run the workspace tests, and verify the crate count**

Run: `cargo check --workspace --all-targets && make test_nts && grep -c 'Mirror of' crates/sapi/src/types.rs`
Expected: green, and the grep prints 0.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -S -m "refactor(sapi)!: fold extension_api and rapira_runtime into rapira_sapi and delete the mirror types"
```

---

### Task 3: Work, Sink and Intake inside the SAPI crate

**Files:**
- Create: `crates/sapi/src/work.rs`
- Modify: `crates/sapi/src/types.rs` (delete `Unit`, `Job`, `GrpcJob`), `crates/sapi/src/handler.rs` (becomes the `Sink`), `crates/sapi/src/start.rs` (the queue carries `Box<dyn Work>`), `crates/sapi/src/exchange/mod.rs` (`Held` moves to `work.rs`; `Front` becomes `DispatcherClasses`), `crates/sapi/src/exchange/receive.rs`, `crates/sapi/src/exchange/grpc.rs`, `crates/sapi/src/rapira_worker.rs`, `crates/sapi/src/classic_worker.rs`, `crates/sapi/src/runtime.rs` (`RapiraBackend` submits through `Intake`)
- Create: `crates/sapi/src/http/exchange.rs` (the `Exchange` unit) and `crates/sapi/src/grpc/call.rs` (the `Call` unit), with `pub mod http; pub mod grpc;` in `lib.rs`
- Test: `crates/sapi/src/work.rs` (in-crate, no PHP), `crates/tests/tests/dispatcher_loop.rs`, `crates/tests/tests/grpc_dispatcher.rs` (existing, must stay green)

**Interfaces:**
- Consumes: `types::Frame`, `types::Request`, `types::Context`, `api::{UnaryCall, UnaryReply, RpcStatus}` from Task 2.
- Produces:

```rust
// crates/sapi/src/work.rs
use std::ffi::CStr;
use crate::{zend_class_entry, zend_object};

/// The cycle bookkeeping view of a unit that receive() handed out.
pub trait Held {
    fn finalized(&self) -> bool;
    fn host_closed(&self) -> bool;
    fn discard(&mut self);
    fn is_finalized(&self) -> bool {
        self.finalized() || self.host_closed()
    }
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

/// The queue from a plugin thread to the PHP thread. Clone per plugin task.
#[derive(Clone)]
pub struct Sink { /* SyncSender<Box<dyn Work>>, Arc<AtomicUsize> pending */ }

impl Sink {
    /// pending is incremented before the send: the consumer decrements as soon as it wakes.
    pub async fn submit(&self, unit: Box<dyn Work>) -> Result<(), Refused>;
}

/// The typed handle a plugin's transport submits to.
#[derive(Clone)]
pub struct Intake<U: Work> { /* enum { Sink(Sink), Channel(tokio::sync::mpsc::Sender<U>) } */ }

impl<U: Work> Intake<U> {
    pub fn new(sink: Sink) -> Self;
    /// A test intake with no PHP thread. The receiver plays the PHP thread.
    pub fn channel(cap: usize) -> (Self, tokio::sync::mpsc::Receiver<U>);
    pub async fn submit(&self, unit: U) -> Result<(), Refused>;
}
```

`Refused` keeps the `Display` text of today's `HandleError` and the same wait: `INTAKE_WAIT` of 30 s for `Saturated`. `Intake::channel` maps a full channel to a `send().await` and a closed channel to `Stopped`; it applies `Saturated` only when `send()` has not completed within `INTAKE_WAIT`.

```rust
// crates/sapi/src/http/exchange.rs
pub struct Exchange { /* req: types::Request, tx: Option<Sender<Frame>> */ }
impl Exchange {
    /// `rx` is the reply the transport reads. Stamps received_at when the transport left it None.
    pub fn new(req: crate::types::Request) -> (Self, tokio::sync::mpsc::Receiver<Frame>);
}
impl Work for Exchange { /* cancelled: tx.is_closed(); attach: ExchangeState::new(req, tx) as today; into_cgi: Some(Context::new(req, tx, true)); shed: 503 head + End as Unit::shed did */ }

// crates/sapi/src/grpc/call.rs
pub struct Call { /* call: api::UnaryCall, received_at: f64, reply: oneshot::Sender<api::UnaryReply> */ }
impl Call {
    pub fn new(call: crate::api::UnaryCall) -> (Self, tokio::sync::oneshot::Receiver<crate::api::UnaryReply>);
}
impl Work for Call { /* cancelled: reply.is_closed(); attach: GrpcState::new(self) as today; into_cgi: None; shed: UNAVAILABLE outcome as Unit::shed did */ }
```

Each unit module also exports its dispatcher classes, which Task 4 hands to `start_worker` and Task 6 and 7 move into the plugin crates:

```rust
// crates/sapi/src/http/mod.rs
pub static DISPATCHER_CLASSES: DispatcherClasses = DispatcherClasses {
    dispatcher: || unsafe { crate::rapira_ce_internal_http_dispatcher },
    info: || unsafe { crate::rapira_ce_internal_http_dispatcher_info },
    unit: || unsafe { crate::rapira_ce_internal_http_exchange },
    busy: c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
};
// crates/sapi/src/grpc/mod.rs: the same with the grpc entries and the UnaryCall message.
```

The frame capacity stays 4 (`FRAME_CAP`). The dispatcher-mode `receive()` allocates from `DispatcherClasses::unit`, pulls a `Box<dyn Work>`, skips it when `cancelled()`, and calls `attach`. The thread-local `FRONT` and the `Front` enum are deleted; `start_worker` takes `DispatcherClasses` and sets a thread-local on the PHP thread. `worker_main` no longer matches on a gRPC mode: `Mode::GrpcDispatcher` is deleted and the grpc services move to a `OnceLock<Vec<GrpcService>>` in `crates/sapi/src/grpc/mod.rs` that `set_services` fills before the PHP thread starts (Task 4 moves that call to the plugin's `prepare`). `rapira_worker::next_job` calls `into_cgi()` and treats `None` as a bug, as the `unreachable!` did.

- [ ] **Step 1: Write the failing intake tests**

Add to `crates/sapi/src/work.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;
    impl Work for Probe {
        fn cancelled(&self) -> bool { false }
        unsafe fn attach(self: Box<Self>, _: *mut zend_object) -> *mut dyn Held { unreachable!() }
        fn into_cgi(self: Box<Self>) -> Option<crate::types::Context> { None }
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
        tokio::time::advance(INTAKE_WAIT + std::time::Duration::from_secs(1)).await;
        assert_eq!(second.await.unwrap().unwrap_err(), Refused::Saturated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sink_counts_pending_until_the_consumer_pulls() {
        let (sink, rx, pending) = Sink::for_test(1);
        sink.submit(Box::new(Probe)).await.unwrap();
        assert_eq!(pending.load(std::sync::atomic::Ordering::Relaxed), 1);
        let _ = rx.recv().unwrap();
        pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pending.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
```

`Sink::for_test(cap) -> (Sink, std::sync::mpsc::Receiver<Box<dyn Work>>, Arc<AtomicUsize>)` is `#[cfg(test)]` in `work.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rapira_sapi work::`
Expected: compile error, `Intake` and `Sink` are not defined.

- [ ] **Step 3: Implement work.rs, the two units, and rewire the PHP thread**

Move `Held` and `release` from `exchange/mod.rs` into `work.rs` and re-export them from `exchange` for the code that still lives there. Move the body of `RapiraHandle::enqueue` into `Sink::submit`, the intake creation in `start_worker` to produce a `Sink`, and delete `RapiraHandle`, `HandleError`, `Unit`, `Job`, `GrpcJob`, `Unit::shed`, `Unit::into_http`, `Unit::front`. `Rapira::sink(&self) -> Sink` replaces `Rapira::handle()`. `runtime::RapiraBackend` holds an `Intake<Exchange>` and an `Intake<Call>`, both `Intake::new(rapira.sink())`, and its `exec` and `unary` build the unit, submit, and return the receiver. `refused()` maps `Refused` to `Rejected` as it mapped `HandleError`.

- [ ] **Step 4: Run the sapi unit tests, then the workspace**

Run: `cargo test -p rapira_sapi work:: && make test_nts`
Expected: the four new tests pass; the dispatcher and grpc suites in `crates/tests` pass unchanged.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -S -m "refactor(sapi): carry work units through one intake and set the dispatcher classes per worker"
```

---

### Task 4: Plugin, Worker and run_plugin replace Extension, ExtensionRuntime, Backend and Php

**Files:**
- Create: `crates/sapi/src/plugin.rs`
- Delete: `crates/sapi/src/api.rs` (`Extension`, `Backend`, `Php`, `Reply`, `Rejected` move or die as listed), `crates/sapi/src/runtime.rs` (replaced by `plugin.rs`), `crates/net/src/lib.rs` `ServerThread`
- Modify: `crates/sapi/src/types.rs` (`Mode` becomes plain), `crates/sapi/src/start.rs` (`start_worker(mode, entrypoint, hooks, classes)`), `crates/sapi/src/lib.rs`
- Modify: `crates/plugins/http/src/lib.rs`, `serve.rs`, `handler.rs`, `bridge.rs`, `request.rs`; `crates/plugins/grpc/src/lib.rs`, `serve.rs`, `dispatch.rs`
- Modify: `src/main.rs`, `src/worker.rs`
- Modify: `crates/tests/src/lib.rs`, `crates/tests/src/grpc.rs`, `crates/tests/tests/extension_tests.rs` (rename to `plugin_tests.rs`), `crates/tests/tests/grpc_server.rs`, `crates/tests/tests/e2e/grpc.rs`, `crates/tests/tests/e2e/streaming.rs`
- Test: `crates/tests/tests/plugin_tests.rs`, `crates/tests/tests/grpc_server.rs`, `crates/plugins/http/src/handler.rs` and `bridge.rs` in-crate tests, `crates/config/src` for the mode check

**Interfaces:**
- Consumes: `work::{Sink, Intake, DispatcherClasses, Refused}`, `http::Exchange`, `grpc::Call` from Task 3, `rapira_net::{PrepareCtx, PreparedListener, Acceptor, Serve, Stop, StopHandle}`.
- Produces:

```rust
// crates/sapi/src/plugin.rs
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Classic,
    Worker,
    Dispatcher,
}

pub struct Worker {
    pub handle: tokio::runtime::Handle,
    pub sink: crate::work::Sink,
    /// Set to true once. The plugin stops accepting, drains within `grace`, and returns from `serve`.
    pub stop: tokio::sync::watch::Receiver<bool>,
    pub grace: Duration,
    pub entrypoint: PathBuf,
    pub mode: Mode,
}

pub trait Plugin: Send + 'static {
    /// The TOML section and the dispatcher name PHP sees.
    fn name(&self) -> &'static str;
    /// The pool modes this plugin serves. The root refuses another mode at boot.
    fn modes(&self) -> &'static [Mode];
    /// The classes receive() allocates from. None for a plugin with no dispatcher surface. Task 5 folds this into `php()`.
    fn dispatcher_classes(&self) -> Option<crate::work::DispatcherClasses>;
    /// Master side, before the fork, no runtime.
    fn prepare(&mut self, ctx: &mut rapira_net::PrepareCtx) -> anyhow::Result<()>;
    /// Worker side, on the plugin thread. Returns after the stop signal and the drain.
    fn serve(self: Box<Self>, worker: Worker) -> anyhow::Result<()>;
}

pub struct Running { /* thread JoinHandle, stop Sender, runtime */ }

impl Running {
    /// Sets the stop flag. The plugin drains on its own time.
    pub fn stop(&self);
    pub fn stopper(&self) -> Stopper;
    /// Joins the plugin thread. An error from serve, a panic, or a join past `grace` after stop is an Err.
    pub fn join(self) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct Stopper(tokio::sync::watch::Sender<bool>);
impl Stopper { pub fn stop(&self); }

/// Builds one two-worker tokio runtime with the IO and time drivers, spawns `rapira-{name}` and runs `serve` on it.
pub fn run_plugin(plugin: Box<dyn Plugin>, sapi: &crate::Rapira, grace: Duration, entrypoint: PathBuf, mode: Mode) -> anyhow::Result<Running>;
```

`Rapira::start_worker(mode: Mode, entrypoint: PathBuf, hooks: WorkerHooks, classes: Option<DispatcherClasses>)` replaces the path-carrying `Mode`. In classic mode `entrypoint` is the script the SAPI runs per request, as `set_script` did; `set_script` is called by `start_worker` and its public fn is deleted. The C global `rapira_mode` is set from the plain enum.

The http plugin: `Config` loses nothing; `Server::serve` does `let intake = Intake::<Exchange>::new(worker.sink.clone());`, adopts the listener with `Acceptor::adopt(prepared, stop_handle, &rt)` where `rt` is a `tokio::runtime::Runtime` no longer built here: the accept loop needs `&Runtime` today only for `rt.enter()` and `block_on(drain)`, so change `Acceptor::adopt` and `Acceptor::run` to take a `tokio::runtime::Handle`, and run `drain` with `handle.block_on`. The stop eventfd `rapira_net::Stop` is created in `serve` and fired by a task that awaits `worker.stop`. The handler calls `intake.submit(exchange)` and reads frames from the receiver. `Rejected` moves to `crates/plugins/http/src/handler.rs` as the plugin's own pre-dispatch refusal `{ status: u16, reason: String }`, built from `Refused` (503 Saturated, 500 Stopped) and from the multipart parser (400, 413). The multipart parse moves into the http handler path before `submit`, with the same `spawn_blocking` on `worker.handle`. `RuntimeOptions.uploads` becomes `Config.uploads: Option<multipart::Limits>` and `Config.sendfile_root: PathBuf`, both set by the root from the settings for this task, and the plugin calls `rapira_sapi::set_sendfile_root` in `serve`.

The grpc plugin: the same shape with `Intake<Call>`; `PhpDispatcher` holds the intake; `Php::unary` calls become `intake.submit(call)` plus `rx.await`. `Ok(None)` stays the lost-call outcome.

Both plugins get a second constructor for tests: `Server::with_intake(config: Config, intake: Intake<Exchange>) -> Self` and `Server::with_intake(config: Config, intake: Intake<Call>) -> Self`. `serve` uses the injected intake when present and `Intake::new(worker.sink.clone())` otherwise. This is the only test seam a plugin exposes.

The root `src/worker.rs`: `worker_body(env, plugin: Box<dyn Plugin>, args: PoolArgs)` keeps the exit code protocol, the quota hooks, the lifeline watch, the spool dir, and calls `run_plugin`; the signal thread (`sigset`, `wait_signal`, the second-signal exit 131) moves here from `runtime.rs`. `PoolArgs.http` stays for this task and goes in Task 8. `src/main.rs` keeps one builder per plugin, returning `Box<dyn Plugin>`, and checks `plugin.modes().contains(&mode)` before `prepare`, failing with `"{name}.pool.mode = {mode}: this plugin serves {modes}"`.

- [ ] **Step 1: Write the failing mode check test**

In `src/main.rs` tests:

```rust
#[test]
fn a_pool_mode_the_plugin_does_not_serve_fails_the_boot() {
    let plugin = rapira_grpc::Server::init(grpc_test_config());
    let err = check_mode("grpc", &plugin, rapira_sapi::plugin::Mode::Worker).unwrap_err();
    assert_eq!(err.to_string(), "grpc.pool.mode = worker: this plugin serves dispatcher");
}
```

`check_mode(name: &str, plugin: &dyn Plugin, mode: Mode) -> anyhow::Result<()>` lives in `src/main.rs`. `grpc_test_config()` builds a `rapira_grpc::Config` with the echo descriptor set from `crates/tests/fixtures/grpc/echo.binpb` and `listen = ListenAddr::Tcp(127.0.0.1:0)`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rapira_core a_pool_mode_the_plugin_does_not_serve_fails_the_boot`
Expected: compile error, `check_mode` and `modes` do not exist.

- [ ] **Step 3: Implement plugin.rs, convert both plugins, the root and the tests**

Conversion of the test doubles, all by the same rule "the test plays the PHP thread":

- `crates/tests/tests/grpc_server.rs`: `FakePhp` and its `Answer` enum go. `grpc::start` builds `let (intake, rx) = Intake::<Call>::channel(16); Server::with_intake(config, intake)`, and the test's answering task pulls `Call` units from `rx`, and for each one calls `call.respond(UnaryReply { .. })`, or drops it for the lost-call case, or leaves it queued for the deadline case. Add `impl Call { pub fn method(&self) -> &str; pub fn metadata(&self) -> &HeaderMap; pub fn deadline(&self) -> Option<f64>; pub fn message(&self) -> &Bytes; pub fn respond(self, reply: UnaryReply); }` to `crates/sapi/src/grpc/call.rs` for this. The pre-dispatch refusal cases use `Intake::channel(1)` and never drain, so the plugin sees `Saturated` after `INTAKE_WAIT`; pause tokio time in those tests as today's tests do for deadlines.
- `crates/plugins/http/src/handler.rs` tests: `NoPhp` becomes `Intake::<Exchange>::channel(1)` whose receiver is dropped; `Scripted` becomes a channel whose receiver the test pulls, then sends the scripted frames on the exchange's sender through `Exchange::reply_sender(&mut self) -> Sender<Frame>` (a `pub(crate)`-free accessor on the unit, needed because the test is the PHP side). Add that accessor to `Exchange`.
- `crates/plugins/http/src/lib.rs` lifecycle test: replace the `Arc::strong_count(&backend) == 1` assertion with `Arc::strong_count(&sink_probe) == 1` where the probe is an `Arc<()>` cloned into the intake through `Intake::channel`; if that is not expressible, assert that the channel receiver observes `None` after shutdown, which proves every sender is dropped.
- `crates/tests/tests/extension_tests.rs` becomes `plugin_tests.rs`: each test `Extension` becomes a `Plugin` whose `serve` submits through `Intake::<Exchange>::new(worker.sink.clone())` and returns when `worker.stop` fires; `ExtensionRuntime::run` becomes `run_plugin(Box::new(plugin), &rapira, grace, entrypoint, mode)`; `Running::join` and `stop` keep their meaning.
- `crates/tests/src/lib.rs`: `submit` builds `Exchange::new(req)` and submits through `Intake::<Exchange>::new(rapira.sink())`; `call` does the same with `Call`. `set_script` calls become the `entrypoint` argument of `Rapira::start_worker`.

- [ ] **Step 4: Build and run the whole suite, including e2e**

Run: `cargo check --workspace --all-targets && make test`
Expected: green. Confirm `grep -rn 'Backend\b\|ExtensionRuntime\|ServerThread\|extension_api' src crates --include=*.rs` prints only the tower-http `Backend` in `crates/middleware/static_files/src/cache.rs`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -S -m "refactor(sapi)!: replace Extension, ExtensionRuntime and Backend with Plugin, Work and Intake"
```

---

### Task 5: The build helper and the MINIT registry, with the parts still inside the SAPI crate

**Files:**
- Create: `crates/php_build/Cargo.toml`, `crates/php_build/src/lib.rs`
- Modify: `crates/sapi/build.rs` (uses the helper, exports the include dir), `crates/sapi/Cargo.toml` (`links = "rapira_sapi"`, `build-dependencies` gains `rapira_php_build`)
- Split: `crates/sapi/rapira_classes.c` into `rapira_classes.c` (base) plus `rapira_http_classes.c` and `rapira_grpc_classes.c`; `crates/sapi/rapira_http.c` into the three base `__construct` shells (move into `rapira_classes.c`) and the http shells; `crates/sapi/wrapper.h` into `rapira_sapi.h` (base decls, the `RAPIRA_MODE_*` and `RAPIRA_HANDLE_*` enums, `rapira_throw_or_backstop`, the base class entries) plus `rapira_http.h` and `rapira_grpc.h` (the object layouts and class entries of each plugin)
- Modify: `crates/sapi/module.c` (MINIT calls `rapira_register_classes()` then `rapira_rs_register_plugin_classes()`), `crates/sapi/src/start.rs` (`boot_master(parts: &[PhpPart])`), `crates/sapi/src/lib.rs`
- Modify: `crates/sapi/rapira_dispatcher.c` (the `receive`, `tryReceive`, `getInfo`, `pendingCount`, `activeCount` bodies become exported functions `rapira_sapi_receive(INTERNAL_FUNCTION_PARAMETERS)` and so on; the `Rapira_Internal_Http_Dispatcher` methods move to `rapira_http_classes.c` as one-line calls to them)
- Modify: `src/main.rs` (passes the parts), `crates/tests/src/lib.rs` (the harness boots with both parts)
- Test: `crates/tests/tests/php_ext_tests.rs` or a new `crates/tests/tests/registry.rs`

**Interfaces:**
- Produces:

```rust
// crates/php_build/src/lib.rs
pub struct Php {
    pub includes: Vec<String>,
    pub lib_dirs: Vec<String>,
    pub version: (u32, u32),
}

/// Runs `php-config` (or `$PHP_CONFIG`) and fails with its stderr when it cannot run.
pub fn discover() -> anyhow::Result<Php>;

/// Compiles `files` with cc into a static library named `name`, with the PHP include dirs and `extra_includes`, and defines RAPIRA_VERSION from CARGO_PKG_VERSION.
pub fn compile(name: &str, files: &[&str], php: &Php, extra_includes: &[&str]);

/// Emits rerun-if-changed for each file and for PATH and PHP_CONFIG.
pub fn rerun_if_changed(files: &[&str]);
```

`crates/sapi/build.rs` calls `discover`, runs bindgen as today, calls `compile("rapira_sapi", &[...base files...], &php, &[])`, emits `cargo:rustc-link-lib=dylib=php`, the link search dirs, the `php84`/`php85` cfgs, and `println!("cargo:include={}", manifest_dir)`. With `links = "rapira_sapi"` in `Cargo.toml` a plugin's build script reads `DEP_RAPIRA_SAPI_INCLUDE`.

```rust
// crates/sapi/src/plugin.rs
/// One plugin's PHP surface.
#[derive(Clone, Copy)]
pub struct PhpPart {
    /// Registers the plugin's classes. Runs in MINIT after the base classes.
    pub register: unsafe extern "C" fn(),
    pub dispatcher: Option<crate::work::DispatcherClasses>,
}

pub trait Plugin: Send + 'static {
    // as in Task 4, with `dispatcher_classes` replaced by:
    /// The plugin's PHP surface. None for a plugin without one.
    fn php(&self) -> Option<PhpPart>;
}

// crates/sapi/src/start.rs
/// MINIT once in the master. Base classes first, then each part in order.
pub fn boot_master(parts: &[PhpPart]) -> anyhow::Result<PhpModule>;
```

The root collects `plugin.php()` of every configured plugin into the `parts` slice, and passes `plugin.php().and_then(|p| p.dispatcher)` to `start_worker`.

The parts are stored in a `static PARTS: OnceLock<Vec<PhpPart>>` before `php_module_startup`, and `rapira_rs_register_plugin_classes()` (a `#[unsafe(no_mangle)] extern "C"` fn) iterates them. In this task the http and grpc parts live in the SAPI crate as `rapira_sapi::http::PHP_PART` and `rapira_sapi::grpc::PHP_PART`, each with its own `register` C function (`rapira_http_register_classes`, `rapira_grpc_register_classes`) and its `DispatcherClasses`. The two plugins return them from `Plugin::php`.

- [ ] **Step 1: Write the failing registry test**

In `crates/tests/tests/registry.rs`:

```rust
use tests::php_lock;

/// The plugin classes extend the base ones, so the order base then parts must hold in MINIT.
#[test]
fn every_plugin_class_exists_after_boot_with_both_parts() {
    let _g = php_lock();
    let module = rapira_sapi::boot_master(&[rapira_sapi::http::PHP_PART, rapira_sapi::grpc::PHP_PART]).unwrap();
    for class in ["Rapira\\Work", "Rapira\\Http\\Exchange", "Rapira\\Internal\\Http\\Exchange", "Rapira\\Grpc\\UnaryCall", "Rapira\\Internal\\Grpc\\UnaryCall"] {
        assert!(rapira_sapi::class_exists(class), "{class} missing");
    }
    drop(module);
}
```

`rapira_sapi::class_exists(name: &str) -> bool` looks the lowercase name up in `EG(class_table)` through `zend_hash_str_find_ptr`; add it to `crates/sapi/src/zend.rs`. If the harness already has a boot helper that returns a live module, use it and keep the assertion.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p tests --test registry`
Expected: compile error, `boot_master` takes no parts and `PHP_PART` does not exist.

- [ ] **Step 3: Implement the helper, the split and the registry**

Keep every C shell body byte-identical apart from its file. The three base constructors (`Rapira_InetAddress`, `Rapira_UnixAddress`, `Rapira_Tls`) go to `rapira_classes.c`. The class entry globals, the object handlers and the `register_*` calls of each plugin go to that plugin's `_classes.c`. The Rust side of each plugin's classes (`values.rs` ctor functions for form field, uploaded file, multipart, request; `exchange/request.rs`, `respond.rs`, `sendfile.rs`, `grpc.rs`) stays in place for this task.

- [ ] **Step 4: Run the registry test, then the suite**

Run: `cargo test -p tests --test registry && make test_nts`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -S -m "feat(build): add rapira_php_build and register plugin classes through a MINIT registry"
```

---

### Task 6: The http plugin owns its PHP surface

**Files:**
- Move: `crates/sapi/rapira_http.stub.php`, `rapira_http_arginfo.h`, `rapira_http.c` (http shells), `rapira_http_classes.c`, `rapira_http.h`, `rapira_exchange.c` to `crates/plugins/http/`
- Move: `crates/sapi/src/exchange/request.rs`, `respond.rs`, `sendfile.rs`, `headers.rs` (if only http uses it), the `ExchangeState` and its helpers from `exchange/mod.rs`, `crates/sapi/src/http/exchange.rs`, `crates/sapi/src/multipart.rs`, and the http constructors from `values.rs` to `crates/plugins/http/src/php/` and `src/`
- Create: `crates/plugins/http/build.rs`
- Modify: `crates/plugins/http/Cargo.toml` (`build-dependencies = { rapira_php_build }`), `crates/plugins/http/src/lib.rs` (`php()` returns the crate's part), `crates/sapi/src/lib.rs` (make `pub` every item the moved code uses), `crates/sapi/build.rs` (drop the moved files), `Makefile` (no change, the glob finds the stub), `crates/tests/Cargo.toml` and `src/lib.rs` (the harness imports `rapira_http::Exchange`)
- Test: `crates/tests/tests/dispatcher_loop.rs`, `basic_tests.rs`, `worker_mode.rs`, e2e `static_files.rs` and `streaming.rs` (existing, must stay green)

**Interfaces:**
- Consumes: `rapira_php_build`, `rapira_sapi::{PhpPart, DispatcherClasses, Work, Held, release, guard, zend, cgi types}`.
- Produces: `rapira_http::PHP_PART: PhpPart`, `rapira_http::Exchange` (the unit), `rapira_http::multipart::{Limits, parse, ...}` as today's API.

- [ ] **Step 1: Move the files**

```bash
git mv crates/sapi/rapira_http.stub.php crates/sapi/rapira_http_arginfo.h crates/sapi/rapira_http.c crates/sapi/rapira_http_classes.c crates/sapi/rapira_http.h crates/sapi/rapira_exchange.c crates/plugins/http/
mkdir -p crates/plugins/http/src/php
git mv crates/sapi/src/exchange/request.rs crates/plugins/http/src/php/request.rs
git mv crates/sapi/src/exchange/respond.rs crates/plugins/http/src/php/respond.rs
git mv crates/sapi/src/exchange/sendfile.rs crates/plugins/http/src/php/sendfile.rs
git mv crates/sapi/src/http/exchange.rs crates/plugins/http/src/exchange.rs
git mv crates/sapi/src/multipart.rs crates/plugins/http/src/multipart.rs
```

`crates/plugins/http/build.rs`:

```rust
fn main() -> anyhow::Result<()> {
    let php = rapira_php_build::discover()?;
    let sapi_include = std::env::var("DEP_RAPIRA_SAPI_INCLUDE")?;
    rapira_php_build::compile(
        "rapira_http_php",
        &["rapira_http.c", "rapira_http_classes.c", "rapira_exchange.c"],
        &php,
        &[&sapi_include],
    );
    rapira_php_build::rerun_if_changed(&[
        "rapira_http.c",
        "rapira_http_classes.c",
        "rapira_exchange.c",
        "rapira_http.h",
        "rapira_http.stub.php",
        "rapira_http_arginfo.h",
    ]);
    Ok(())
}
```

The C files include `rapira_sapi.h` for the base declarations and `rapira_http.h` for their own. The Rust side declares the plugin's class entries and object layouts itself:

```rust
// crates/plugins/http/src/php/mod.rs
use rapira_sapi::{zend_class_entry, zend_object};

unsafe extern "C" {
    pub static mut rapira_ce_http_request: *mut zend_class_entry;
    pub static mut rapira_ce_internal_http_exchange: *mut zend_class_entry;
    // one line per class entry the moved Rust code reads
    pub fn rapira_http_register_classes();
}

/// Mirrors `rapira_exchange_obj` in rapira_http.h. The C fields sit before `std`.
#[repr(C)]
pub struct ExchangeObj {
    pub job: *mut std::ffi::c_void,
    pub std: zend_object,
}

pub static PHP_PART: rapira_sapi::plugin::PhpPart = rapira_sapi::plugin::PhpPart {
    register: rapira_http_register_classes,
    dispatcher: Some(rapira_sapi::work::DispatcherClasses {
        dispatcher: || unsafe { rapira_ce_internal_http_dispatcher },
        info: || unsafe { rapira_ce_internal_http_dispatcher_info },
        unit: || unsafe { rapira_ce_internal_http_exchange },
        busy: c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
    }),
};
```

The sapi crate exports what the moved code calls: `pub use callbacks::{guard, MAX_BUFFERED_BODY, send_error_head, joined_field}`, `pub mod zend`, `pub use exchange::{container_of, note_served, note_received, cycle helpers the code calls}`, `pub use work::{Held, release}`, the timer park helpers that `respond.rs` uses (`zend_set_timeout`, `zend_unset_timeout` re-exports and the `armed_at` handling), and the `types` module. Make an item `pub` only when the moved code references it. Do not add new helpers.

- [ ] **Step 2: Build and run the suite**

Run: `cargo check --workspace --all-targets && make test`
Expected: green. Then confirm `ls crates/sapi/*.c` lists only `module.c`, `wrapper.c`, `rapira_classes.c`, `rapira_dispatcher.c`, and `ls crates/sapi/src/exchange/` lists only `mod.rs`, `receive.rs`, `headers.rs` if shared, and `tests.rs` reduced to what still lives there.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -s -S -m "refactor(http)!: own the Rapira\\Http PHP surface in the http plugin crate"
```

---

### Task 7: The grpc plugin owns its PHP surface

**Files:**
- Move: `crates/sapi/rapira_grpc.stub.php`, `rapira_grpc_arginfo.h`, `rapira_grpc.c`, `rapira_grpc_classes.c`, `rapira_grpc.h` to `crates/plugins/grpc/`
- Move: `crates/sapi/src/exchange/grpc.rs` to `crates/plugins/grpc/src/php/call.rs`, `crates/sapi/src/grpc/call.rs` to `crates/plugins/grpc/src/call.rs`, the `GrpcService` and `GrpcMethod` types and the services `OnceLock` to `crates/plugins/grpc/src/schema.rs`
- Create: `crates/plugins/grpc/build.rs` (same shape as the http one, files `rapira_grpc.c`, `rapira_grpc_classes.c`)
- Modify: `crates/plugins/grpc/src/lib.rs` (`php()` returns `PHP_PART`; `prepare` fills the services `OnceLock` from the schema, which replaces `set_services`), `crates/sapi/src/lib.rs`, `crates/sapi/build.rs`, `crates/sapi/src/exchange/receive.rs` (no grpc arm remains), `crates/tests/src/lib.rs` and `src/grpc.rs` (import `rapira_grpc::Call`, `echo_services` builds `rapira_grpc::ServiceInfo`)
- Test: `crates/tests/tests/grpc_dispatcher.rs`, `grpc_values.rs`, `grpc_server.rs`, e2e `grpc.rs` (existing, must stay green)

**Interfaces:**
- Produces: `rapira_grpc::PHP_PART`, `rapira_grpc::Call`, `rapira_grpc::{ServiceInfo, MethodInfo}` as the one service metadata type.

- [ ] **Step 1: Move the files and the types**

Same commands as Task 6 with the grpc names. `Mode::GrpcDispatcher` is gone since Task 4; the services reach PHP through `rapira_grpc::schema::services()` which reads the `OnceLock` that `prepare` filled. The master fills it before the fork, so every worker inherits it.

- [ ] **Step 2: Build and run the suite**

Run: `cargo check --workspace --all-targets && make test`
Expected: green. Confirm `grep -rn 'grpc\|Grpc' crates/sapi/src crates/sapi/*.c crates/sapi/*.h` prints nothing.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -s -S -m "refactor(grpc)!: own the Rapira\\Grpc PHP surface in the grpc plugin crate"
```

---

### Task 8: Each plugin owns its config section

**Files:**
- Create: `crates/plugins/http/src/config.rs`, `crates/plugins/grpc/src/config.rs`
- Modify: `crates/config/src/lib.rs` (delete `Settings.http`, `Settings.grpc`, `FileConfig`, `resolve_http`; keep the shared sections and helpers as `pub`), delete `crates/config/src/http.rs` and `grpc.rs` after their content moved
- Modify: `crates/master/Cargo.toml` and `src/lib.rs` (`Scaling` and `PoolConfig` fields come from `rapira_config::{Scaling, PoolSettings}`; delete the master mirror)
- Modify: `src/main.rs` (the `FileConfig` from the spec, `resolve`, the plugin list), `src/worker.rs` (delete `HttpArgs`, `PoolArgs.http`)
- Modify: `crates/plugins/http/src/lib.rs` (`Config` is built from `Settings` inside the crate; `uploads` and `sendfile_root` live there), `crates/plugins/grpc/src/lib.rs`
- Modify: `crates/tests/tests/e2e/harness.rs` (renders the same TOML, no key changes)
- Test: `crates/plugins/http/src/config.rs` and `crates/plugins/grpc/src/config.rs` in-crate (parsing and validation need no fixture), `crates/config/src/lib.rs` tests move with their subject, `crates/tests/tests/e2e/lifecycle.rs` (existing)

**Interfaces:**
- Consumes: `rapira_config::{PoolSection, PoolSettings, Listen, SupervisorSection, SupervisorSettings, LogSection, LogSettings, Scaling, RunMode, ConfigCtx, parse_duration, resolve_path}` with `ConfigCtx { dir: PathBuf }` for config-relative paths.
- Produces, in each plugin:

```rust
// crates/plugins/http/src/config.rs
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section { /* the fields of today's HttpSection, plus `pool: PoolSection` and `middleware: Vec<String>`, `static: Option<rapira_static_files::Section>` */ }

#[derive(Debug)]
pub struct Settings { /* today's HttpSettings fields, `pool: PoolSettings`, `middleware: Vec<Middleware>` */ }

#[derive(Debug)]
pub enum Middleware { Static(rapira_static_files::Settings) }

/// Boot checks run here: entrypoint file, uploads dir, sendfile root, static root, middleware names.
pub fn resolve(section: Section, ctx: &rapira_config::ConfigCtx) -> anyhow::Result<Settings>;

impl crate::Server {
    pub fn from_settings(settings: Settings, supervisor: &rapira_config::SupervisorSettings) -> Self;
}
```

`rapira_static_files` gains `Section` and `Settings` and `resolve` for `[http.static]`. The error texts stay: unknown names, duplicates, a listed name without its table, a table without its name, with the full key such as `http.middleware`.

- [ ] **Step 1: Move the config tests with their subject**

The tests in `crates/config/src/lib.rs:615-630` for middleware names and every test that parses an `[http]` or `[grpc]` table move to the plugin's `config.rs` and keep their table format:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        toml: &'static str,
        error: &'static str,
    }

    #[test]
    fn middleware_list_and_tables_must_agree() {
        let cases = [
            Case { name: "unknown name", toml: "middleware = [\"staticc\"]\n[pool]\nentrypoint = \"w.php\"", error: "http.middleware: unknown middleware \"staticc\"" },
            Case { name: "listed without table", toml: "middleware = [\"static\"]\n[pool]\nentrypoint = \"w.php\"", error: "http.static: required when \"static\" is listed" },
            Case { name: "table without listing", toml: "[static]\nroot = \".\"\n[pool]\nentrypoint = \"w.php\"", error: "http.static: configured but not listed in http.middleware" },
            Case { name: "duplicate", toml: "middleware = [\"static\", \"static\"]\n[static]\nroot = \".\"\n[pool]\nentrypoint = \"w.php\"", error: "http.middleware: \"static\" listed twice" },
        ];
        for case in cases {
            let section: Section = toml::from_str(case.toml).unwrap();
            let err = resolve(section, &ctx()).unwrap_err().to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }
}
```

Use the exact error strings the current `crates/config/src/http.rs:118-146` produces; copy them, do not invent new ones.

- [ ] **Step 2: Run the moved tests to verify they fail**

Run: `cargo test -p rapira_http config::`
Expected: compile error, no `Section` in the crate.

- [ ] **Step 3: Implement the split and the root composition**

`src/main.rs`:

```rust
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    http: Option<rapira_http::config::Section>,
    grpc: Option<rapira_grpc::config::Section>,
    #[serde(default)]
    supervisor: rapira_config::SupervisorSection,
    #[serde(default)]
    log: rapira_config::LogSection,
}

struct Settings {
    http: Option<rapira_http::config::Settings>,
    grpc: Option<rapira_grpc::config::Settings>,
    supervisor: rapira_config::SupervisorSettings,
    log: rapira_config::LogSettings,
}

fn resolve(path: &Path) -> anyhow::Result<Settings>;
```

The "no plugin configured" guard stays in `resolve` with the same message. Each plugin's `pool.mode` is checked against `plugin.modes()` (Task 4). `PoolArgs` keeps `mode`, `entrypoint`, `max_requests`, `grace` only.

- [ ] **Step 4: Run the suite including e2e**

Run: `cargo check --workspace --all-targets && make test`
Expected: green. Confirm `grep -n 'fn listen_addr\|UnsafeFieldNames::\|GrpcService {' src/main.rs` prints nothing: the five config mirrors are gone.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -S -m "refactor(config)!: let each plugin own its config section"
```

---

### Task 9: Tower layers for [http].middleware and [grpc].interceptors

**Files:**
- Delete: `crates/sapi/src/middleware.rs` (`Middleware`, `Handler`, `Next`, `Protocol`, `Peer` move or die as listed)
- Modify: `crates/plugins/http/src/handler.rs` (the chain is applied as tower layers), `crates/plugins/http/src/serve.rs`, `crates/plugins/http/src/lib.rs` (`Config.middleware: Vec<Layer>`), `crates/plugins/http/Cargo.toml` (`tower = { version = "0.5", features = ["util"] }`)
- Modify: `crates/middleware/static_files/src/lib.rs` (a `tower::Layer` whose service answers a hit and forwards a miss), `Cargo.toml`
- Modify: `crates/plugins/grpc/src/config.rs` (`interceptors: Vec<String>`), `crates/plugins/grpc/src/serve.rs` (`Vec<Interceptor>` applied around `MapResponse::new(service, status_in_trailers)`), `crates/plugins/grpc/src/lib.rs`
- Modify: `examples/rapira.toml` (`interceptors = []` under `[grpc]` with a comment that none ships)
- Test: `crates/middleware/static_files/src/lib.rs` in-crate chain tests (convert from `Next` to `tower::ServiceExt::oneshot`), `crates/plugins/http/src/handler.rs` chain tests, `crates/plugins/grpc/src/config.rs` and `serve.rs` tests, e2e `static_files.rs`

**Interfaces:**
- Produces:

```rust
// crates/plugins/http/src/middleware.rs
use std::convert::Infallible;
pub type Body = http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, BoxError>;
pub type Request = http::Request<Body>;
pub type Response = http::Response<Body>;
pub type Service = tower::util::BoxCloneService<Request, Response, Infallible>;
/// One middleware: a layer over the plugin's inner service. Applied outermost first in config order.
pub type Layer = tower::util::BoxLayer<Service, Request, Response, Infallible>;

/// The peer of the connection, in the request extensions. A middleware may replace it.
#[derive(Debug, Clone)]
pub struct Peer { pub remote: Addr, pub server: Addr, pub https: bool, pub received_at: f64 }

// crates/middleware/static_files/src/lib.rs
pub struct StaticFiles { /* as today */ }
impl StaticFiles {
    pub fn layer(self) -> rapira_http::middleware::Layer;
}

// crates/plugins/grpc/src/interceptor.rs
pub type Interceptor = tower::util::BoxLayer<connectrpc-service-type, http::Request<B>, http::Response<ConnectRpcBody>, Error>;
```

For the grpc alias use the exact request and response types that `MapResponse::new(self.service.clone(), status_in_trailers)` produces; read them from `connectrpc::ConnectRpcService` and name them in the alias. The private in-flight state of the http plugin stays in the request extensions as today, and the `ReqState` missing check keeps answering 500.

- [ ] **Step 1: Write the failing tests**

`crates/plugins/grpc/src/config.rs`:

```rust
#[test]
fn any_interceptor_name_fails_the_boot_because_none_ships() {
    let section: Section = toml::from_str("interceptors = [\"auth\"]\ndescriptor_set = \"echo.binpb\"\n[pool]\nentrypoint = \"w.php\"").unwrap();
    let err = resolve(section, &ctx()).unwrap_err().to_string();
    assert_eq!(err, "grpc.interceptors: unknown interceptor \"auth\"");
}
```

`crates/plugins/grpc/src/serve.rs`:

```rust
/// A test layer that adds a response header proves the chain wraps the service.
#[tokio::test]
async fn interceptors_wrap_the_service_outermost_first() {
    // build Serving with two interceptors that append "x-trace: a-in"/"b-in" on the way in and "-out" on the way out,
    // send one request through the assembled service with tower::ServiceExt::oneshot,
    // assert the header order ["a-in", "b-in", "b-out", "a-out"] as the old chain test did.
}
```

`crates/middleware/static_files/src/lib.rs`: convert `chain_runs_outermost_first_and_unwinds_in_reverse`, `short_circuit_skips_downstream_and_the_handler` and `empty_chain_reaches_the_handler_directly` from `Next::new(chain, handler)` to `layers.into_iter().rev().fold(inner, |s, l| l.layer(s))` plus `oneshot`, with the same assertions.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rapira_grpc interceptor && cargo test -p rapira_static_files`
Expected: compile errors, `interceptors` and `Layer` do not exist.

- [ ] **Step 3: Implement the layers**

The http plugin converts its hyper `RapiraService` into a tower service with `tower::service_fn` per connection, folds the layers, and hands the result to hyper through `hyper_util::service::TowerToHyperService`. The empty-chain fast path stays: with no middleware the plugin serves the hyper service directly. `Peer` is inserted before the chain and read by the terminal service.

- [ ] **Step 4: Run the suite including e2e**

Run: `cargo check --workspace --all-targets && make test`
Expected: green. Confirm `grep -rn 'Protocol::Http\|Next::new\|dyn Middleware' crates src --include=*.rs` prints nothing.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -s -S -m "feat(http,grpc)!: run [http].middleware and [grpc].interceptors as tower layers"
```

---

### Task 10: Docs

**Files:**
- Modify: `CONTRIBUTING.md` (crate table, one sentence per plugin kind, the PHP-free rule is gone), `README.md` (any `extension` wording), `crates/plugins/http/README.md`, `crates/plugins/grpc/README.md`, `examples/rapira.toml`
- Modify: the two plugin READMEs get a "Crate layout" section that lists the files of the spec's plugin crate layout and a "Config" section with the section's keys.

- [ ] **Step 1: Update the docs**

Search and fix every stale word:

```bash
grep -rn -i 'extension_api\|rapira_runtime\|php_sys\|ExtensionRuntime\|native extension\|\bfront\b\|\bhost\b' README.md CONTRIBUTING.md crates/plugins/*/README.md examples/
```

Each hit is either renamed to the plugin vocabulary or deleted. One paragraph per line, no em-dashes, STE.

- [ ] **Step 2: Verify the workspace one last time**

Run: `make test && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p tests --features e2e --tests -- -D warnings`
Expected: green.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -s -S -m "docs: describe the plugin crate layout"
```
