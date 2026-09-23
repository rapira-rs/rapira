# rapira_http

The HTTP front built into the `rapira` binary. It terminates HTTP/1.1 on the configured listener and answers every request from PHP through the `extension_api` bridge; there is no upstream and nothing is proxied.

## How it works

`Server` implements `extension_api::Extension`. The host's extension runtime runs without an IO driver, so this crate serves on its own tokio runtime (two workers, thread `rapira-http`) on a dedicated thread. The listener is bound pre-fork by the master through `PrepareCtx`; each worker inherits the fd and adopts it with `from_std`.

Connections are served by hyper's http1 builder. Each request runs through admission checks, the middleware chain, and the PHP handler: the request maps to an `extension_api::Request`, executes via `Php::exec`, and the resulting `ReplyEvent` stream is written back to the socket as it arrives.

## Middleware

`Config.middleware` holds an `extension_api::Middleware` chain, outermost first, shared by every protocol this plugin serves. A middleware sees `http::Request<Body>`/`http::Response<Body>` with `Protocol` and `Peer` in the request extensions, and either calls `next.run(req)` or answers on its own. The extensions also carry private per-request state for the handler. Keep the extensions when you rebuild a request. Do not retain them past the call. Admission checks run before the chain. A request that fails them does not reach the middleware.

Built-in middleware lives under `crates/middleware`, one crate per middleware. The `middleware` list in `[http]` selects the built-in middleware and sets the chain order. `rapira_static_files::StaticFiles` serves files from `[http.static].root`. A miss falls through the chain to PHP. A permission error or a bad file name is also a miss. Any other read failure answers 500. That request does not reach PHP.

`StaticFiles` holds served files in memory. An entry stays fresh for one second. A rewritten file therefore reaches the client after that time. Each worker keeps its own cache of up to 16 MiB, and the cache stores no file above 256 KiB. The memory cost is 16 MiB for each process in `http.pool.processes`. A permission change does not stop the cache from serving a file. To stop it, delete the file or replace it. A restart empties the cache.

## Request handling

- The request body is buffered before dispatch and capped by `max_body_size`: a declared `Content-Length` over the cap answers 413 before the body is read, an over-long streamed body answers 413 when the cap is crossed.
- `Expect: 100-continue` is honored by hyper: the interim 100 goes out when the body is first read and is skipped entirely when the request is refused first.
- `Host` rules per RFC 9112 §3.2: a repeated, missing or empty `Host` on HTTP/1.1 answers 400. https://www.rfc-editor.org/rfc/rfc9112#section-3.2
- Absolute-form request-targets are accepted. The target's host information replaces `Host`, without the userinfo part. PHP sees the origin-form path and query; the request target keeps the full form. https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
- `CONNECT` answers 501; this front implements no tunnels.
- A request body that makes no read progress for `keepalive_timeout` answers 408 and closes the connection.
- `unsafe_field_names` guards the CGI variable mapping: a field name with `_` or `.` lands on the `$_SERVER` entry a `-` name owns, so such names are dropped (default) or the request answers 400 (`"reject"`).
- Header field names reach PHP lowercased, one entry per field line, values in wire order per name.

## Response handling

- Responses stream; nothing is buffered beyond one frame per request plus the PHP-side frame channel.
- Framing belongs to the front: PHP-supplied `Content-Length`/`Transfer-Encoding` and hop-by-hop fields are stripped (RFC 9110 §7.6.1, including fields named by a `Connection` value), a PHP-declared length becomes the `Content-Length`, and hyper authors chunked framing when there is none. https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1
- A truncated reply (worker death, or a body shorter than the declared length) drops the connection without a clean terminator, so the client can tell the response was cut short.
- `sendFile` events stream the validated file slice in 64 KiB reads off the blocking pool.
- Interim (1xx) heads and trailers are dropped.
- `write_timeout` bounds a single stalled write toward the client; a connection that makes no write progress for that long is closed and the PHP unit discarded.

## Shutdown

On the stop signal the accept loop ends immediately, idle keepalive connections close, and in-flight requests drain within `drain_grace`; requests still running past the deadline are cut short and reported.

## Configuration

`Extension::init(config)` receives everything; `rapira serve` resolves `rapira.toml` and defaults into this struct. It then registers the extension.

| Field                | Meaning                                                                    |
| -------------------- | -------------------------------------------------------------------------- |
| `listen`             | TCP address or unix socket path                                            |
| `server_name`        | what PHP sees as `SERVER_NAME`                                             |
| `server_port`        | what PHP sees as `SERVER_PORT`                                             |
| `max_body_size`      | request body cap in bytes; over it answers 413                             |
| `write_timeout`      | bound on a single stalled write, not a whole-response deadline             |
| `keepalive_timeout`  | closes an idle keepalive connection; also bounds head and body-frame reads |
| `drain_grace`        | shutdown drain window; must expire before the host escalates its stop      |
| `unsafe_field_names` | `Drop` or `Reject` for names aliasing a CGI variable                       |
| `superglobals`       | whether a `$_SERVER` mapping exists to protect; `Drop` is inert without it |
| `middleware`         | the shared middleware chain, outermost first                               |

## Build

```sh
cargo build -p rapira_http
cargo clippy -p rapira_http --all-targets
```

## License

MIT, see [LICENSE](../../../LICENSE).
