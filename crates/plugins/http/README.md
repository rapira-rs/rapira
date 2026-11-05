# rapira_http

The http plugin of the `rapira` binary. It terminates HTTP/1.1 on the configured listener and answers every request from PHP. It has no upstream and proxies nothing.

## How it works

`Server` implements `rapira_sapi::plugin::Plugin`:

- `name()` returns `http`: the TOML table and the dispatcher name that PHP sees.
- `modes()` accepts the classic, worker and dispatcher modes.
- `dispatcher()` returns `DISPATCHER_CLASSES`, the dispatcher classes of dispatcher mode.
- The crate exports `rapira_http_register_classes`. MINIT calls it after the base classes to register the `Rapira\Http` classes.
- `prepare()` runs in the master before the fork, with no runtime. It sets the server identity and the `sendFile()` root that the workers inherit, removes the spool dirs of dead workers and binds the listener with `rapira_net::bind`.
- `serve()` runs in the worker on the plugin thread `rapira-http`. It gets a `Worker` from the SAPI: the handle of a tokio runtime with two worker threads, the `Intake` to the PHP thread, the stop flag and the drain bound `drain_grace`. After the stop, the plugin drains within `worker.drain_grace` and returns.

`serve()` takes the worker's `Intake`. It adopts the inherited listener with `rapira_net::Acceptor` and runs the accept loop on the plugin thread. hyper's http1 builder serves each connection on the runtime. Each request goes through the admission checks, the middleware chain and the inner service. The inner service reads the body and submits the unit to the intake: a `Context` with the CGI view in the classic and worker modes, an `Exchange` in dispatcher mode. PHP writes the reply as frames (`Head`, `Chunk`, `File`, `End`), and the plugin writes them to the socket as they arrive.

`Exchange` is the dispatcher-mode unit. It implements `rapira_sapi::work::Work`:

- `cancelled()` returns true when the client is gone. `receive()` skips such a unit.
- `into_held()` gives the `ExchangeState` that the `Rapira\Http\Exchange` object from `receive()` holds in dispatcher mode. The SAPI keeps the state in the object, answers `isFinalized()` and `isCancelled()` from it, and reclaims it when the object is freed. When the object is freed before the unit is settled, the state's `lost()` sends a 500 head, or a truncated End after a sent head.
- `shed()` answers 503 when the PHP boot of the worker failed.

The client gets 503 when the intake of the worker stays full for 30 seconds or when the waiting request bodies use the 64 MiB body budget of the worker, and 500 when the PHP thread of the worker has stopped. PHP does not see these requests.

## Crate layout

- `Cargo.toml`: depends on `rapira_sapi`, `rapira_net`, `rapira_config` and `rapira_static_files`. `rapira_php_build` is a build dependency.
- `build.rs`: compiles the C files with `rapira_php_build::compile` against the PHP headers. `rapira_http.h` includes `rapira_sapi.h` by its path in `crates/sapi`.
- `rapira_http.stub.php`: the PHP stub of the `Rapira\Http` and `Rapira\Internal\Http` classes.
- `rapira_http_arginfo.h`: generated from the stub by `make stubs`. Do not edit it.
- `rapira_http.h`: the C layout of the exchange object and the class entries.
- `rapira_http_classes.c`: `rapira_http_register_classes`, the object handlers, and the method shells of the dispatcher classes. MINIT calls `rapira_http_register_classes` after the base classes.
- `rapira_http.c`: the method shells of the value classes `Request`, `FormField`, `UploadedFile` and `Multipart`.
- `rapira_exchange.c`: the method shells of `Exchange`.
- `src/lib.rs`: `Config`, `Server` and its `Plugin` impl.
- `src/config.rs`: `Section` (the `[http]` table), `Settings`, `resolve` with the boot checks, and `Server::from_settings`.
- `src/serve.rs`: the accept loop, one hyper connection per accepted socket, and the drain.
- `src/handler.rs`: the admission checks, the middleware chain, the body read, the submit to the intake and the response head.
- `src/middleware.rs`: the chain types `Body`, `Request`, `Response`, `Service`, `Layer`, and `Peer`.
- `src/check.rs`: the admission checks: `Host`, field names, declared length.
- `src/request.rs`: builds the SAPI `Request` from the HTTP request and the `Peer`.
- `src/response.rs`: the response fields and the error responses.
- `src/bridge.rs`: the reply body that streams the PHP frames and the `sendFile` slices, and the write timeout.
- `src/exchange.rs`: the `Exchange` unit and its `Work` impl.
- `src/multipart.rs`: the multipart parser and the spool dirs of dispatcher mode.
- `src/php/`: the Rust behind the PHP methods: the class entries, `DISPATCHER_CLASSES` and the declaration of `rapira_http_register_classes` (`mod.rs`), `getRequest()` (`request.rs`), the head, body and trailer writes (`respond.rs`, `headers.rs`), `sendFile()` (`sendfile.rs`), the server identity (`info.rs`), the value class constructors (`values.rs`), and the unit tests of these parts that need no PHP (`tests.rs`).

## Middleware

`[http].middleware` lists the middleware names in chain order. The plugin builds one tower layer for each name and applies the layers around its inner service, the first listed outermost. Without middleware, hyper serves the inner service directly.

A layer is `middleware::Layer`: a `tower::util::BoxCloneServiceLayer` over `http::Request<Body>` and `http::Response<Body>`, with the error type `Infallible`. `middleware::Body` is an `UnsyncBoxBody<Bytes, BoxError>`. A layer answers a request itself or calls the inner service.

The request extensions hold private state of the plugin: the peer (the remote address, the server address, `https` and the receive time, which PHP gets), the request authority and the in-flight counter of the drain. A layer that rebuilds a request must keep the extensions. A layer must not keep them after the call.

The admission checks run before the chain. A request that fails them does not reach the middleware.

To add a built-in middleware:

- Put its layer in a new crate under `crates/middleware`.
- In the root `Cargo.toml`, add the crate to the workspace `members` and to `[workspace.dependencies]`.
- Add the crate to the dependencies in `crates/plugins/http/Cargo.toml`.
- In `src/config.rs`, add its table to `Section`, as `r#static` is for `[http.static]`.
- Resolve the table in `settings` and store the result in `Settings`.
- In `check_middleware`, check its name and require its table. Reject a configured table that the list does not name.
- Add its boot check to `resolve`, as `check_static_root` is for `static`.
- Build its layer in `Server::from_settings`. Keep the configured list order when more than one middleware is available.
- Add cases for the new name to the tables of `middleware_list_and_tables_must_agree` and of a resolve test.
- Add an e2e test in `crates/tests/tests/e2e/` that starts `rapira` with the new `[http.<name>]` table through `Spawn::http_extra`, and add its `mod` line to `main.rs`.
- Add a commented example of the table to `examples/rapira.toml`, next to `[http.static]`.
- Add its boot errors to the error list in the Config section.

`rapira_http` depends on each middleware crate, so a crate under `crates/middleware` cannot use `Peer`. A middleware that needs `Peer` must be in `rapira_http`, or `Peer` must first move to a shared crate.

`rapira_static_files::StaticFiles` is the `static` middleware. It serves files from `[http.static].root`. A miss goes to the next layer, and to PHP when `static` is the last layer. A permission error or a bad file name is also a miss. Any other read failure answers 500. That request does not reach PHP.

`StaticFiles` holds served files in memory. An entry stays fresh for one second. A rewritten file therefore reaches the client after that time. Each worker keeps its own cache of up to 16 MiB, and the cache stores no file above 256 KiB. The memory cost is 16 MiB for each process in `http.pool.processes`. A permission change does not stop the cache from serving a file. To stop it, delete the file or replace it. A restart empties the cache.

## Request handling

- The request body is buffered before dispatch and capped by `max_body_size_mb`: a declared `Content-Length` over the cap answers 413 before the body is read, an over-long streamed body answers 413 when the cap is crossed.
- `Expect: 100-continue` is honored by hyper: the interim 100 goes out when the body is first read and is skipped entirely when the request is refused first.
- A missing or empty `Host` on HTTP/1.1, and more than one `Host` line in any version, answer 400 (RFC 9112 §3.2); the `Host` value is the authority. https://www.rfc-editor.org/rfc/rfc9112#section-3.2
- In dispatcher mode, a multipart request with more than one `Content-Type` line answers 400: the plugin and a PHP consumer could split the body on different boundaries (RFC 9110 §8.3). https://www.rfc-editor.org/rfc/rfc9110#section-8.3
- An absolute-form request-target is accepted as it is: the authority comes from `Host`, PHP sees the origin-form path and query in `uri` and the full form in `target`.
- A request body that makes no read progress for `keepalive_timeout_secs` answers 408 and closes the connection.
- In the classic and worker modes a field name with a byte outside `A-Z`, `a-z`, `0-9` and `-` is dropped before dispatch: the CGI mapping rewrites `_` and `.` to `_`, so such a name would land on the `$_SERVER` entry of a `-` name. nginx does the same by default.
- Header field names reach PHP lowercased, one entry per field line, values in wire order per name.

## Response handling

- Responses stream; nothing is buffered beyond one frame per request plus the PHP-side frame channel.
- The plugin owns the framing: PHP-supplied `Content-Length`/`Transfer-Encoding` and hop-by-hop fields are stripped (RFC 9110 §7.6.1, including fields named by a `Connection` value), a PHP-declared length becomes the `Content-Length`, and hyper authors chunked framing when there is none. https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1
- A truncated reply (worker death, or a body shorter than the declared length) drops the connection without a clean terminator, so the client can tell the response was cut short.
- `sendFile` events stream the validated file slice in 256 KiB reads off the blocking pool.
- Interim (1xx) heads and trailers are dropped.
- `write_timeout_secs` bounds a single stalled write toward the client; a connection that makes no write progress for that long is closed and the PHP unit discarded.

## Shutdown

On the stop signal the accept loop ends immediately, idle keepalive connections close, and in-flight requests drain within the drain window (see Config); requests still running past the deadline are cut short and reported.

## Config

The `[http]` table. Unknown keys fail the boot.

- `listen`: `host:port`, `:port` for all interfaces, or `unix:/path`. Default `127.0.0.1:8000`.
- `server_name`: what PHP sees as `SERVER_NAME`. Default `localhost`.
- `server_port`: what PHP sees as `SERVER_PORT`. Default: the listen port, or 80 on a unix socket.
- `max_body_size_mb`: the request body limit. Default 8. A larger body answers 413.
- `write_timeout_secs`: the limit for one stalled write to the client, not for the whole response. Default 30.
- `keepalive_timeout_secs`: closes an idle keepalive connection, and limits each head read and body-frame read. Default 60.
- `middleware`: the middleware names in chain order, the first listed outermost. Default: none.
- `[http.uploads]`: the multipart limits of dispatcher mode: `dir` (default: the system temp dir), `max_file_size_mb` (2), `max_field_size_kb` (256), `max_files` (20), `max_parts` (1024), `max_part_headers` (32). This table under another mode fails the boot.
- `[http.sendfile]`: `root`, the directory that must contain each `sendFile()` path. Default: the entrypoint directory.
- `[http.static]`: the settings of the `static` middleware: `root` (required, an existing directory) and `forbid` (the file-name suffixes it never serves, default `[".php"]`).
- `[http.pool]`: the worker pool: `entrypoint` (required), `mode`, `processes`, `max_requests`, `max_memory_mb`, `request_terminate_timeout_secs`. `examples/rapira.toml` shows each key.

The drain window is not a key of this table. It is `[supervisor].process_control_timeout_secs` minus 5 seconds, or minus half of it when it is below 10 seconds. The drain therefore ends before the master sends SIGTERM.

Each of these errors in the middleware list fails the boot with exit code 1:

- `http.middleware lists "<name>" twice`
- `http.middleware entry "<name>" is unknown; known middleware: "static"`
- `http.middleware lists "static" but [http.static] is missing`
- `[http.static] is configured but http.middleware does not list "static"`

## Build

```sh
cargo build -p rapira_http
cargo clippy -p rapira_http --all-targets
```

## License

MIT, see [LICENSE](../../../LICENSE).
