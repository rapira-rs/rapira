# rapira_http

The http plugin of the `rapira` binary. It terminates HTTP/1.1 on the configured listener and answers every request from PHP. It has no upstream and proxies nothing.

## How it works

`Server` implements `rapira_sapi::plugin::Plugin`:

- `name()` returns `http`: the TOML table and the dispatcher name that PHP sees.
- `dispatcher()` returns the address of `rapira_http_plugin`, the C record of the dispatcher surface: the dispatcher classes and the busy error of `receive()`.
- `worker_init()` returns the function that binds the server name and port on each PHP thread before its first entrypoint.
- The crate exports `rapira_http_register_classes`. MINIT calls it after the base classes to register the `Rapira\Http` classes.
- `Server::from_settings` sets the server identity and `sendFile()` root, removes spool directories of dead server processes and binds the listener before PHP starts.
- `serve()` runs on the shared IO runtime. It receives the HTTP pool's `Intake`, the stop flag and the connection drain timeout.

`PreparedListener::run` accepts connections. Each request goes through the admission checks and the middleware chain. The inner service reads the body and submits the unit to the pool: a `Context` in classic and worker modes, or an `ExchangeState` in dispatcher mode. Any available worker can serve it, including a later request on the same connection. PHP writes reply frames (`Head`, `Chunk`, `File`, `End`), which the plugin writes to the socket as they arrive.

`ExchangeState` in `src/php/mod.rs` is the dispatcher-mode unit. It implements `rapira_sapi::work::Held`:

- `cancelled()` returns true when the client is gone. `receive()` skips such a unit.
- The SAPI keeps the state in the `Rapira\Http\Exchange` object from `receive()`, answers `isFinalized()` and `isCancelled()` from it, and reclaims it when the object is freed.
- `lost()` sends a 500 head, or a truncated End after a sent head, when the object is freed before the unit is settled.
- `shed()` answers 503 during a boot retry if the pool has no ready worker.

The HTTP pool shares 1,024 intake slots and a 64 MiB budget for pending request bodies. If the pool cannot reserve the body bytes, the client gets 503 immediately. If no slot becomes free within 30 seconds, the client also gets 503. PHP does not see these requests.

## Crate layout

- `Cargo.toml`: depends on `rapira_sapi`, `rapira_net`, `rapira_config`, `rapira_http_middleware`, `rapira_static_files`, and `rapira_compress`. `rapira_php_build` is a build dependency.
- `build.rs`: compiles the C files with `rapira_php_build::compile` against the PHP headers. `rapira_http.h` includes `rapira_sapi.h` by its path in `crates/sapi`.
- `rapira_http.stub.php`: the PHP stub of the `Rapira\Http` and `Rapira\Internal\Http` classes.
- `rapira_http_arginfo.h`: generated from the stub by `make stubs`. Do not edit it.
- `rapira_http.h`: the C layout of the exchange object, the class entries and the server identity.
- `rapira_http_classes.c`: the `rapira_http_plugin` record, `rapira_http_register_classes`, the object handlers, and the method shells of `serverName()` and `serverPort()` of the dispatcher info. MINIT calls `rapira_http_register_classes` after the base classes.
- `rapira_http.c`: the constructors of the value classes `Request`, `FormField`, `UploadedFile` and `Multipart`.
- `rapira_exchange.c`: the method shells of `Exchange`.
- `src/lib.rs`: `Config`, `Server` and its `Plugin` impl.
- `src/config.rs`: `Section` (the `[http]` table), `Settings`, `resolve` with the boot checks, and `Server::from_settings`.
- `src/serve.rs`: the accept loop, dedicated single-threaded `LocalSet` IO shards running `hyper` 1.x, and the drain.
- `src/io.rs`: the per-connection `ShardIo` wrapper for keepalive timeouts, write timeouts, and response flush completion.
- `src/handler.rs`: the admission checks, the middleware chain, the body read, and the submit to the intake.
- `src/check.rs`: the admission checks: `Host`, field names, declared length.
- `src/request.rs`: builds the SAPI `Request` from the HTTP request and the `Peer`.
- `src/responder.rs`: the PHP-to-IO response slot, large-buffer pool, `ResponseBody`, middleware body encoding, and error responses.
- `src/multipart.rs`: the multipart parser and the spool dirs of dispatcher mode.
- `src/php/`: the Rust behind the PHP methods: the class entries, the declarations of `rapira_http_plugin` and `rapira_http_register_classes` and the write of the server identity (`mod.rs`), `getRequest()` (`request.rs`), the head, body and trailer writes (`respond.rs`, `headers.rs`), `sendFile()` (`sendfile.rs`), and the unit tests of these parts that need no PHP (`tests.rs`).

## Middleware

`[http].middleware` lists the middleware names in chain order, the first listed outermost. `Config.middleware` holds `Vec<Arc<dyn Middleware>>` from `rapira_http_middleware` (re-exported as `rapira_http::middleware`).

Each middleware implements `Middleware` and declares its active lifecycle stages via `phases()` (`Phases::REQUEST`, `Phases::RESPONSE_HEAD`, or both). `on_request(&self, req: &mut RequestCtx)` runs outer to inner on the IO shard and returns `RequestStep::Next`, `RequestStep::Respond(ShortResponse)`, or `RequestStep::Async(...)`. `on_response_head(&self, is_head, extensions, head: &mut ResponseHead)` runs inner to outer before wire framing (only for middlewares strictly outside a short-circuiting stage) and can return a `Box<dyn BodyEncoder>` to transform response body bytes. `RequestCtx` provides `method`, `uri`, `version`, `headers`, `peer`, and per-request `extensions`.

The admission checks run before the chain. A request that fails them does not reach the middleware.

To add a built-in middleware:

- Put its `Middleware` implementation in a new crate under `crates/middleware`, depending on `rapira_http_middleware`.
- In the root `Cargo.toml`, add the crate to the workspace `members` and to `[workspace.dependencies]`.
- Add the crate to the dependencies in `crates/plugins/http/Cargo.toml`.
- In `src/config.rs`, add its table to `Section`, as `r#static` is for `[http.static]` and `compress` is for `[http.compress]`.
- Resolve the table in `settings` and store the result in `Settings`.
- In `check_middleware`, register its name in `rapira_config::check_stages` and enforce whether its config table is required or optional. Reject a configured table that the list does not name.
- Add any boot checks to `resolve`, as `check_static_root` is for `static`.
- Construct `Arc<dyn Middleware>` in `Server::from_settings`. Keep the configured list order when more than one middleware is available.
- Add cases for the new name to the tables of `middleware_list_and_tables_must_agree` and of a resolve test.
- Add an e2e test in `crates/tests/tests/e2e/` that starts `rapira` with the new `[http.<name>]` table through `Spawn::http_extra`, and add its `mod` line to `main.rs`.
- Add a commented example of the table to `examples/rapira.toml`, next to `[http.static]`.
- Add its boot errors to the error list in the Config section.

`rapira_static_files::StaticFiles` is the `static` middleware. It serves files from `[http.static].root`. A miss goes to the next middleware, and to PHP when `static` is the last middleware. A permission error or a bad file name is also a miss. Any other read failure answers 500. That request does not reach PHP.

`StaticFiles` holds served files in memory. All HTTP connections share one cache of up to 16 MiB. It stores files of at most 256 KiB. An entry stays fresh for one second. A permission change does not stop the cache from serving a file. To stop it, delete the file or replace it. A server restart empties the cache.

`rapira_compress::Compress` is the `compress` middleware. It negotiates `zstd` or `gzip` from `Accept-Encoding` and encodes eligible responses inline. Place `"compress"` before `"static"` in `http.middleware` (for example `middleware = ["compress", "static"]`) to compress static file responses as well as PHP responses. `[http.compress]` is optional when `"compress"` is listed and uses default settings when omitted.

## Request handling

- The request body is buffered before dispatch and capped by `max_body_size_mb`: a declared `Content-Length` over the cap answers 413 before the body is read, an over-long streamed body answers 413 when the cap is crossed.
- In dispatcher mode the plugin parses a `multipart/form-data` body while it arrives. It writes each file part to the spool directory and holds only the field parts in memory.
- The plugin reads a connection in pieces of at most 64 KiB. A request head that does not fit into this read buffer answers 431.
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

On stop, the listener closes. A connection accepted before the stop can finish a request during the drain, including its first request. The response carries `Connection: close`. Idle connections can stay open until the drain deadline. At the deadline the plugin closes remaining connections and reports any incomplete requests.

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
- `[http.compress]`: optional override settings of the `compress` middleware: `min_size` (default 1024 bytes), `gzip_level` (1..=9, default 4), and `zstd_level` (1..=19, default 3).
- `[http.pool]`: `entrypoint` (required), `mode`, `workers`, `max_requests`, `max_boot_failures`, `request_terminate_timeout_secs`, `execution_clock` and `env`. See the [example configuration](../../../examples/rapira.toml).

The connection drain lasts for `[server].stop_timeout_secs` minus five seconds, or half the stop timeout if it is below ten seconds. The remaining time is for PHP shutdown. See [Run the server](../../../README.md#run-the-server) for request timeouts and exit codes.

`[http.pool.env]` supplies environment variables to PHP. `getenv()` first reads changes from `putenv()`, then the pool table, then the environment at server startup. `putenv('NAME')` hides a name. `putenv('NAME=')` sets an empty value. Changes last for one request in classic mode and for one app instance in worker and dispatcher modes. `${NAME}` in the text of `parse_ini_file()` and `parse_ini_string()` reads the same values. After `putenv('NAME')`, `${NAME}` reads the environment at server startup.

The boot `$_SERVER` and `$_ENV` (when `variables_order` contains `E`) import the PHP environment. [PHP checks `HTTP_PROXY` against the process environment](https://github.com/php/php-src/blob/php-8.6.0/main/php_variables.c#L885-L897) in both arrays. The `$_SERVER` of an HTTP request gets the pool entries and the request variables. A request variable with the same name wins.

Native libraries and child processes read the process environment, and `putenv()` does not change it. `exec()`, `shell_exec()`, `system()`, `passthru()`, `popen()` and `proc_open()` with no environment argument give the child the process environment. To give a child the pool values and the `putenv()` changes, pass `getenv()` as the environment argument of `proc_open()`. [PHP leaves a variable with an empty value out of that environment](https://github.com/php/php-src/blob/php-8.6.0/ext/standard/proc_open.c#L186-L192). A name that is empty or contains `=` or NUL, or a value that contains NUL, fails the boot.

Symfony Process can give a child only the variables whose names are also keys of `$_SERVER` or `$_ENV`. The `$_SERVER` of a request has no process variables, so such a child can have no `PATH` and no `HOME`. Add these names to `[http.pool.env]`, or put `E` into `variables_order`.

A pool `TMPDIR` or a `putenv()` call can change the value of `getenv('TMPDIR')`. To select the PHP temporary directory that `sys_get_temp_dir()` returns, set [`sys_temp_dir`](https://www.php.net/manual/en/filesystem.configuration.php#ini.sys-temp-dir) in `php.ini` or set `TMPDIR` before starting Rapira. The INI setting takes precedence. For dispatcher uploads, set `dir` in `[http.uploads]`.

GD reads `GDFONTPATH` from the process environment. `setlocale()` with an empty locale name reads `LC_ALL`, the `LC_*` variables and `LANG` from it. Set these variables before starting Rapira, or pass a full font path and a locale name.

Each of these errors in the middleware list fails the boot with exit code 1:

- `http.middleware lists "<name>" twice`
- `http.middleware entry "<name>" is unknown; known middleware: "static", "compress"`
- `http.middleware lists "static" but [http.static] is missing`
- `[http.static] is configured but http.middleware does not list "static"`
- `[http.compress] is configured but http.middleware does not list "compress"`

## Build

```sh
cargo build -p rapira_http
cargo clippy -p rapira_http --all-targets
```

## License

MIT, see [LICENSE](../../../LICENSE).
