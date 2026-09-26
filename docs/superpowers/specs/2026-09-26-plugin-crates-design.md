# Plugin crates design

Date: 2026-09-26. Branch: `refactor/plugin-crates`. Status: approved in discussion, implementation in one draft PR.

## Goal

A plugin author or a middleware author finds one place that says what to implement, with few concepts and one vocabulary. Every boundary type exists once. Adding a plugin means one new crate and one config table. Nothing PHP-visible changes: the PHP contract and every stub stay as they are.

## Decisions

- Plugins stay in this repository. There is no out-of-tree plugin API.
- The settled rules stay: NTS only, one interpreter per forked worker, one plugin per pool and per worker process, master single-threaded without tokio, MINIT once in the master before the fork.
- The one word is `plugin`. The trait is `Plugin`. The directory is `crates/plugins`. The words `extension`, `front` and `host` leave the code and the docs.
- The SAPI crate is `rapira_sapi` in `crates/sapi`. It replaces `php_sys`, `extension_api` and `rapira_runtime`.
- Each plugin crate owns its config section, its PHP stub, its C method shells, the Rust behind those methods, its unit type and its transport.
- The http plugin runs `[http].middleware`. The grpc plugin runs `[grpc].interceptors`. Both are tower layers over `http::Request`. The word `middleware` is HTTP only.
- No fake PHP seam exists in the SAPI. A plugin test drives the plugin's intake from the test, in the role of the PHP thread.
- The Windows fork is out of scope. The PR description names it.

## Plugin kinds

- A dispatcher plugin owns a pool and produces work units that PHP pulls with `receive()`: http, grpc, and later queues. It brings its own PHP unit class, as `Rapira\Http\Exchange` and `Rapira\Grpc\UnaryCall` do today.
- A capability plugin owns no pool. A pool enables it, every worker of that pool initializes it after the fork, and PHP acquires it through the contract's second acquisition path: later kv and a richer logger. This PR adds no capability plugin and no trait for one. The `Plugin` trait leaves room for it: a capability plugin would return `None` from `php` and own no listener.
- A middleware or an interceptor attaches to one plugin's connection pipeline, ordered by config, and never touches PHP.

## Crates after the rework

| Crate | Directory | Role |
|---|---|---|
| `rapira_php_build` | `crates/php_build` | Build helper: `php-config` discovery, C compile of method shells. A build dependency of `rapira_sapi` and of every plugin with a PHP surface. |
| `rapira_net` | `crates/net` | Listeners: `ListenAddr`, `PrepareCtx`, `PreparedListener` (bind in the master before the fork), `Acceptor` (adopt and accept in the worker). No runtime of its own. |
| `rapira_sapi` | `crates/sapi` | The embed SAPI, boot, the PHP thread, the intake, the base PHP contract, the classic and worker modes, the registry, the plugin thread (`run_plugin`, `Running`, `Stopper`), the `Plugin` and `Work` traits. |
| `rapira_config` | `crates/config` | The shared config shapes: `supervisor`, `log`, `pool`, `listen`, duration and path helpers, strict parsing helpers. |
| `rapira_master` | `crates/master` | Unchanged, except that it uses `rapira_config::Scaling`. |
| `rapira_scoreboard` | `crates/scoreboard` | Unchanged. |
| `rapira_http` | `crates/plugins/http` | The http plugin, self-contained. |
| `rapira_grpc` | `crates/plugins/grpc` | The grpc plugin, self-contained. |
| `rapira_static_files` | `crates/middleware/static_files` | An http middleware as a tower layer. |
| `tests` | `crates/tests` | Integration and e2e suites. |
| `rapira_core` | `src/` | The binary: CLI, config composition, the plugin list, boot, fork, and the worker entry `worker_body(env, plugin, args)` in `src/worker.rs`. |

Deleted: `crates/api`, `crates/runtime`, the `Backend` trait and its five test doubles, every mirror type and mapper, `ExtensionRuntime`, `ServerThread`, `PoolArgs.http`, `RuntimeOptions`.

Dependency direction: `config` < `net` < `sapi` < plugins < root. `rapira_config` holds `ListenAddr` and `Mode`, and `rapira_net` and `rapira_sapi` depend on it. `php_build` is a build dependency of `sapi` and of each plugin with a PHP surface. A plugin depends on `sapi`, `net` and `config`. `master` depends on `config` for `Scaling`, which deletes that mirror.

## The SAPI crate

`rapira_sapi` keeps only what php-src forces to be process-wide, plus what every plugin shares.

- Embed and boot: the bindgen bindings, `wrapper.c`, `module.c`, `boot_master` (MINIT once), `start_worker` (the PHP thread), child init, quota, recycle, health, the scoreboard slot, the Zend timer disarm, the bailout guards at the FFI edge, the zval helpers, `Rapira\log`.
- The intake: `Sink`, a bounded queue of `Box<dyn Work>` from the plugin thread to the PHP thread with the pending and active counters, the saturated and stopped refusals, shedding on a failed boot, cancel on drop. A plugin wraps the sink as `Intake<U: Work>`, which is the typed handle its transport submits to. `Intake::<U>::channel()` builds an intake that feeds a typed `mpsc::Receiver<U>` and no PHP thread, so a test can pull units and finalize them in the role of PHP. The test seams are `Intake::channel`, its erased form `Sink::channel`, and each plugin's `Server::with_intake`, which submits to such an intake in place of the worker's sink.
- The base PHP contract: `Rapira\Work`, `Dispatcher`, `DispatcherInfo`, `Mode`, `LogLevel`, `InetAddress`, `UnixAddress`, `Tls`, the exception classes, `get_dispatcher()`, the generic `receive()` and `tryReceive()`, and the object-lifetime reclaim behind them. The stubs `rapira.stub.php` and `rapira_exception.stub.php` and the C file `rapira_classes.c` stay here.
- The classic and worker modes: the CGI request lifecycle, superglobals, `read_post`, the header handler, output buffering. php-src's SAPI has one callback table per process and it is HTTP-shaped by design, so this stays in the SAPI crate. Only a unit whose `Work::into_cgi` returns a context can run in those modes. Today that is the http unit.
- The registry: `PhpPart { register, dispatcher }`. `register` registers a plugin's classes, and MINIT calls it after the base classes. `dispatcher` holds the class entries of the plugin's dispatcher surface. The root passes the list of parts to `boot_master`.
- The PHP thread and the plugin thread: `Rapira::start_worker(mode, entrypoint, hooks, classes)` starts the PHP thread. `run_plugin(plugin, sink, grace, drain_grace, entrypoint, mode)` builds one tokio multi-thread runtime with two workers and the IO and time drivers, and runs `serve` on the plugin thread. It returns `Running`, which stops the plugin and joins it. A join past `grace` after the stop is an error. `Stopper` sets the stop flag from a plain thread. The worker entry `worker_body(env, plugin, args)` in `src/worker.rs` uses them: child init, chdir, `start_worker`, the lifeline watch, `run_plugin`, the signal thread (first QUIT or INT drains, second exits 131), the exit code protocol.
- Shared types every plugin fills: `Addr`, `Tls`, `ClientCert`, and one conversion of each to its PHP object.

The header `rapira_sapi.h` declares what a plugin's C shells need: the base class entries, `rapira_throw_or_backstop`, the object layouts of the base classes. The crate exports its include directory through cargo `links` metadata, so a plugin's `build.rs` finds it.

## The Plugin trait

```rust
pub trait Plugin: Send + 'static {
    /// The TOML section and the dispatcher name PHP sees.
    fn name(&self) -> &'static str;
    /// The pool modes this plugin can serve. A pool with another mode fails the boot.
    fn modes(&self) -> &'static [Mode];
    /// The plugin's PHP surface. None for a plugin without one.
    fn php(&self) -> Option<PhpPart>;
    /// Master side, before the fork, no runtime: bind listeners, validate config, load schemas.
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()>;
    /// Worker side, on the plugin thread. Returns after the stop signal and the drain.
    fn serve(self: Box<Self>, worker: Worker) -> Result<()>;
}
```

`Worker` carries what `run_plugin` built and what the worker entry passed: the tokio `Handle`, the erased `Sink` of the worker's intake that the plugin wraps as `Intake<U>` for its unit type, the stop signal, `drain_grace` (the bound of the plugin's drain after the stop), the entrypoint path, the pool `Mode`. The plugin does not build a runtime, a signal handler or a stop channel.

`prepare` has two flavors, and the trait does not distinguish them. A dispatcher plugin with a listener calls `ctx.bind`. A client plugin validates and resolves, and each worker connects after the fork, because a forked child cannot share a client connection.

## Work units

```rust
pub trait Work: Send + 'static {
    /// The client left while the unit was queued: receive() skips it.
    fn cancelled(&self) -> bool;
    /// Dispatcher mode: attaches the unit to the object receive() allocated from `DispatcherClasses::unit`.
    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held;
    /// The classic and worker modes. None: this unit cannot run there.
    fn into_cgi(self: Box<Self>) -> Option<Context>;
    /// A worker that cannot serve: the plugin's refusal, 503 or UNAVAILABLE.
    fn shed(self: Box<Self>);
}
```

The http unit is `Exchange`: the request, the reply channel of frames (`Interim`, `Head`, `Chunk`, `File`, `End`), the multipart body in dispatcher mode, the CGI view in the other modes. The grpc unit is `Call`: the method, the metadata, the deadline, the message, a oneshot for the outcome. Both live in their plugin crate together with the Rust behind their PHP methods. The intake and the PHP thread see only `Box<dyn Work>`.

## A plugin crate

```
crates/plugins/http/
  Cargo.toml            depends on rapira_sapi, rapira_net, rapira_config, rapira_static_files
  build.rs              rapira_php_build::compile("rapira_http", &["rapira_http.c", "rapira_exchange.c"])
  rapira_http.stub.php  unchanged content
  rapira_http_arginfo.h generated by make stubs
  rapira_http.c         the method shells, unchanged content
  rapira_exchange.c
  src/lib.rs            the Plugin impl
  src/config.rs         Section (serde), Settings, resolve, boot checks
  src/php/              the Rust behind the PHP methods: request, respond, sendfile, the class entries
  src/exchange.rs       the Exchange unit and its Work impl
  src/multipart.rs      the host-side multipart parser
  src/serve.rs          hyper, the middleware chain, the accept loop
```

The grpc crate has the same shape with `rapira_grpc.c`, `src/php/call.rs`, `src/call.rs`, `src/schema.rs` and `src/serve.rs`. `Mode` becomes the plain enum `Classic`, `Worker`, `Dispatcher`, and the entrypoint path travels in `Worker`. `Mode::GrpcDispatcher` disappears: the pool's plugin decides the unit type, and `modes()` says the grpc plugin serves dispatcher mode only. The grpc service list PHP reads through `getServices()` is boot-time metadata the plugin hands to its own PHP classes; it no longer rides on the mode.

The C shells reference the plugin's Rust functions by `extern` declaration, as today. The Rust functions are `#[unsafe(no_mangle)] pub extern "C"` in the plugin crate, and the plugin's build script compiles the shells into the same crate, so the link is inside one crate.

## Config

`rapira_config` keeps the shared shapes and helpers. Each plugin defines its `Section` (serde, `deny_unknown_fields`) and `Settings` and a `resolve(section, ctx) -> Result<Settings>` with the boot checks. The root defines the file shape:

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    http: Option<rapira_http::config::Section>,
    grpc: Option<rapira_grpc::config::Section>,
    #[serde(default)] supervisor: SupervisorSection,
    #[serde(default)] log: LogSection,
}
```

Strictness stays: unknown keys, a listed middleware without its table, a table without its name, duplicates, missing files and directories are boot errors with the full key in the message. Worker-side settings such as the upload limits and the sendfile root live in the http `Settings` and reach the worker inside the plugin, not through the pool arguments.

`[http].middleware` lists names in chain order. The http plugin resolves each name to a layer it knows. `[http.static]` stays. `[grpc].interceptors` lists names in chain order; no interceptor ships in this PR, so every name is an error, and the mechanism has a test layer.

## Middleware and interceptors

Both chains are a `Vec` of tower's `BoxCloneServiceLayer`, applied around the plugin's inner tower service before hyper serves the connection. The http `Layer` is over `http::Request<Body>` and `http::Response<Body>`. The grpc `Interceptor` is over `http::Request<hyper::body::Incoming>` and `http::Response<ConnectRpcBody>`. The peer record travels in the request extensions and a layer may replace it. The in-flight guard travels in the extensions as private state of the http plugin, as today. `rapira_static_files` becomes a `Layer` whose service answers a hit and forwards a miss. The `Middleware`, `Handler`, `Next` and `Protocol` types are deleted.

## Boot and worker sequence

1. The root parses `rapira.toml` into `FileConfig`, resolves the shared sections, and calls each present plugin's `resolve`.
2. The root builds one `Box<dyn Plugin>` per present section and one `PoolSettings` per plugin.
3. One `PrepareCtx` for every pool: the root calls `prepare` on each plugin in order. The master keeps the listener dups.
4. `boot_master(parts)`: MINIT once, base classes then each plugin's classes.
5. `rapira_master::run` forks per pool. The child takes its plugin and calls `worker_body(env, plugin, args)` in `src/worker.rs`.
6. `worker_body`: child init, chdir, `Rapira::start_worker(mode, entrypoint, hooks, classes)`, the lifeline watch, `run_plugin` with the runtime and the plugin thread that runs `serve`, the signal thread. On stop, the plugin drains within `drain_grace` and `serve` returns, and `Running::join` waits at most `grace` after the stop. The worker exits with the code the master expects.

## Tests

- The PHP-behavior suites in `crates/tests` boot the real SAPI as today and change only by path and by name.
- The five `Backend` doubles and the four scripted `ReplySource` types go. Their tests move to `Intake::channel()`: the test pulls the unit and finalizes it the way the PHP thread would. The wire suites for grpc keep their hyper clients and their loopback listener; the grpc plugin's constructor takes an `Intake<Call>` so the test supplies the channel side.
- In-crate unit tests stay for code that needs no fixture, no double and no socket: request checks, header parsing, multipart parsing, schema loading, the chain order.
- E2E stays behind the `e2e` feature and changes by config key and by name only.

## Docs

`CONTRIBUTING.md` gets the new crate table and one sentence per plugin kind. Each plugin README describes the crate layout above and the config table. `examples/rapira.toml` gets `[grpc].interceptors`. The docs site is a separate repository and is out of scope.

## Commits

Each commit builds and passes `make test`.

1. `refactor(sapi)!: rename php_sys to rapira_sapi and move the listener types to net`.
2. `refactor(sapi)!: fold extension_api and rapira_runtime into rapira_sapi and delete the mirror types`.
3. `refactor(sapi): carry work units through one intake and set the dispatcher classes per worker`.
4. `refactor(sapi)!: replace Extension, ExtensionRuntime and Backend with Plugin, Work and Intake`.
5. `feat(build): add rapira_php_build and register plugin classes through a MINIT registry`.
6. `refactor(http)!: own the Rapira\Http PHP surface in the http plugin crate`.
7. `refactor(grpc)!: own the Rapira\Grpc PHP surface in the grpc plugin crate`.
8. `refactor(config)!: let each plugin own its config section`.
9. `feat(http,grpc)!: run [http].middleware and [grpc].interceptors as tower layers`.
10. `docs: describe the plugin crate layout`.

The implementation plan is `docs/superpowers/plans/2026-09-26-plugin-crates.md`.
