# rapira_grpc

The gRPC front built into the `rapira` binary. It serves unary RPCs from PHP over gRPC, gRPC-Web and Connect on one listener. PHP gets each request message as binary protobuf and answers with a binary protobuf message.

## How it works

`Server` implements `extension_api::Extension`. It serves on its own tokio runtime (two workers, thread `rapira-grpc`) on a dedicated thread. The master binds the listener before the fork. Each worker inherits the listener and runs the shared `rapira_net` accept loop on it.

Each connection goes to `serve_connection` of [connect-rust](https://github.com/connectrpc/connect-rust). A connection can use HTTP/1.1 or h2c (HTTP/2 without TLS), over TCP or a unix socket. One listener serves these protocols:

- gRPC over HTTP/2. https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md
- Binary gRPC-Web (`application/grpc-web+proto`). https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-WEB.md
- Connect with proto or JSON messages. https://connectrpc.com/docs/protocol/

connect-rust handles the framing, the compression, the timeout headers and the error encoding. PHP always gets the binary protobuf encoding of the input message. The host transcodes a Connect JSON request to binary protobuf before dispatch, and transcodes the reply back to JSON. The JSON decoder ignores unknown fields, but not an unknown enum value name. A JSON body that does not decode, also one with an enum value name that the descriptor set does not declare, answers INVALID_ARGUMENT, and PHP does not see the call. In binary protobuf, PHP gets an unknown enum value as its number.

- The boot logs a warning for each streaming method of a configured service.
- A streaming method, a method of a service the pool does not serve, and an unknown method answer UNIMPLEMENTED. A Connect unary request (`application/proto` or `application/json`) gets the HTTP status 404. A Connect streaming request (`application/connect+proto` or `application/connect+json`) gets the HTTP status 200 with the error in the end-of-stream message.
- A method with `option idempotency_level = NO_SIDE_EFFECTS;` also accepts a Connect GET request. A GET to any other method answers 405.
- Messages can use gzip compression. A request with a different message encoding answers UNIMPLEMENTED.

## Schemas

The host reads one descriptor set at boot: a binary `google.protobuf.FileDescriptorSet` that contains every imported file. The host routes and transcodes with these descriptors. A changed descriptor set needs a stop and a start of rapira, not a new `rapira` binary. A reload (SIGHUP or SIGUSR2) forks new workers from the master and keeps the old descriptor set.

Build the set with `buf build`. https://buf.build/docs/reference/cli/buf/build/

```sh
buf build --as-file-descriptor-set -o api.binpb
```

Or build it with `protoc`:

```sh
protoc --include_imports --descriptor_set_out=api.binpb -I proto proto/billing/v1/invoice.proto
```

`buf build` includes the imported files by default. `protoc` includes them only with `--include_imports`.

By default, the pool serves the services of the files that no other file of the set imports, in descriptor order. A file that another file imports is a dependency, for example `google/longrunning/operations.proto`, so its services are not served. The host answers `grpc.health.v1.Health` and the reflection services itself, so the pool never serves them. `services` sets the list of fully qualified names, for example `["billing.v1.InvoiceService"]`. Use it when several rapira instances share one set, or to serve a service of an imported file. The master loads the set before the fork, so each of these fails the boot once, with exit code 1:

- a set that rapira cannot read or decode;
- a set without its imports, for example with `unresolved type name ".google.protobuf.Timestamp"`;
- a set that declares no service to serve;
- a `services` entry that is not in the set, or one listed twice, also when one entry has a leading dot;
- a `services` entry that names `grpc.health.v1.Health` or a reflection service.

## The PHP side

The pool runs in dispatcher mode only. `\Rapira\get_dispatcher()` returns a `Rapira\Grpc\GrpcDispatcher`, and `receive()` returns a `Rapira\Grpc\UnaryCall`. The other three call kinds do not occur, because the host serves no streaming method. The PHP contract is in [rapira-rs/contract](https://github.com/rapira-rs/contract/tree/master/src/Grpc).

- `getMessage()` returns the request message. `respond($bytes)` sends the response message. `fail(new Status(StatusCode::NotFound, 'no invoice', $details))` sends an error status.
- `getServices()` lists each configured service with all its methods, streaming methods included.
- A worker holds one call at a time. `receive()` throws `\Error` while the current call is not finalized.
- `getContext()` returns a `Rapira\Grpc\Call\Context`. `$method` is `package.Service/Method`. `$protocol` is `Grpc`, `GrpcWeb` or `Connect`. `$remote` is an `InetAddress`, or a `UnixAddress` on a unix socket. `$receivedAt` is the time when the host has read the whole request message, before it queues the call.

### Metadata

- `Context::$metadata` holds the application metadata. Names are lower case. The values of a name keep their arrival order.
- The host removes the transport names from it: the prefixes `grpc-`, `connect-`, `content-` and `trailer-`, and the names `te`, `trailer`, `connection`, `keep-alive`, `proxy-connection`, `transfer-encoding`, `upgrade`, `host` and `accept-encoding`.
- A text value that is not printable ASCII is dropped. A `-bin` value is split on `,`, then each piece is base64-decoded, with or without padding, and PHP gets the raw bytes. A piece that does not decode is dropped. An empty piece is an empty value.
- `getResponseMetadata()` returns the response headers and trailers of the call. `addHeader()` and `addTrailer()` take a text value: printable ASCII (0x20 to 0x7E). An empty value is permitted. The host removes the leading and trailing spaces of a text value when it sends it, because an HTTP/2 field value must not start or end with whitespace. https://www.rfc-editor.org/rfc/rfc9113#section-8.2.1
- `addBinaryHeader()` and `addBinaryTrailer()` take raw bytes and need a name with the `-bin` suffix. The text methods refuse a name with this suffix. The host sends a `-bin` value as base64 without padding.
- A metadata name uses only `0-9`, `a-z`, `_`, `-` and `.`. A reserved name, an invalid name or a bad value throws `\ValueError`. A repeated name adds a value.
- `respond()` and `fail()` send both halves. For gRPC, the trailers are HTTP/2 trailers. For gRPC-Web, they are in the trailer frame. For Connect, each trailer is a header with the `trailer-` prefix.

### Outcomes

- `fail()` takes the `google.rpc.Status` triple: a code, a message and a list of `ErrorDetail`. For gRPC and gRPC-Web, the host sends `grpc-status`, `grpc-message` and `grpc-status-details-bin`. For Connect, the host sends the HTTP status of the code and a JSON error body. A Connect detail has the bare type name, for example `google.rpc.ErrorInfo`, and an unpadded base64 value.
- `fail()` is the only way to send an error status. The host does not catch `Rapira\Grpc\Exception\GrpcException`. Catch it and call `$call->fail($e->status)`.
- A call that PHP does not finalize is lost. The client gets INTERNAL with the message `internal error`, and the host logs a warning under the `grpc` target. An uncaught throwable also loses the call. The client does not see the message of the throwable.
- The client gets UNAVAILABLE when the host refuses the call before PHP sees it: the intake of the worker stays full for 30 seconds, the pool stops, or the PHP boot of the worker failed.

## Deadlines

A client sets a timeout with `grpc-timeout` (gRPC and gRPC-Web) or `connect-timeout-ms` (Connect). `default_timeout_secs` sets the timeout of a call that has none. `max_timeout_secs` reduces a longer client timeout to this value. Both keys are unset by default, so a call without a client timeout has no deadline.

`Context::$deadline` is the deadline as a Unix timestamp, or null when the call has no deadline. When the deadline passes, the client gets DEADLINE_EXCEEDED and `isCancelled()` returns true. A client that cancels the call or closes the connection has the same effect on PHP.

The host cannot stop PHP code, so PHP continues to run the call. A later `respond()` or `fail()` throws `Rapira\Exception\WorkDiscardedException`. Check `isCancelled()` during long work. Set `default_timeout_secs` so that each call has a deadline.

## Health and reflection

The host serves the gRPC health checking protocol (`grpc.health.v1.Health`) from Rust in each worker. `Check` and `Watch` report SERVING for the empty name `""` and for each configured service. Health does not check PHP: a worker whose PHP boot failed reports SERVING, and its calls get UNAVAILABLE. https://github.com/grpc/grpc/blob/master/doc/health-checking.md

With `reflection = true`, the host also serves server reflection, `grpc.reflection.v1` and `grpc.reflection.v1alpha`. `ListServices` returns the configured services. Each file and symbol of the descriptor set stays resolvable. Reflection is off by default. When it is on, each client can read the full descriptor set. `ListServices` does not list the host's own `grpc.health.v1.Health` service, and the descriptor set does not describe it unless it includes `health.proto`, so a reflection tool needs a protoset (https://github.com/fullstorydev/grpcurl#protoset-files) for `Health/Check`. A Connect JSON `POST` to `/grpc.health.v1.Health/Check` works without one. https://github.com/grpc/grpc/blob/master/doc/server-reflection.md

```sh
grpcurl -plaintext 127.0.0.1:50051 list
```

## Shutdown

On the stop signal the accept loop ends, health reports NOT_SERVING, and open `Watch` streams end. Calls in flight drain within `drain_grace`. Connections that are open after `drain_grace` are cut, and the shutdown reports an error. An open reflection stream, for example an Evans REPL session (https://github.com/ktr0731/evans), holds its connection until `drain_grace` ends.

The listener sends an HTTP/2 keepalive PING to an idle connection every `keepalive_interval` and closes the connection when the peer does not answer within `keepalive_timeout`; `rapira serve` sets both to 10 seconds.

## Limits

- One worker process serves each connection, and a PHP worker runs one call at a time. A gRPC client usually sends all calls of a channel on one HTTP/2 connection. Such a client gets the throughput of one worker, for all pool sizes. The other calls wait in the intake of that worker. To use more workers, open several connections, or use an L7 load balancer that spreads the calls over several connections.
- The message size limit is 4 MiB, and no key changes it. A larger request answers RESOURCE_EXHAUSTED.
- The listener does not terminate TLS. `Context::$tls` is always null. Put a TLS proxy in front of the listener when clients need TLS.
- The JSON decoder has no element memory limit. A 4 MiB JSON request with many small elements can use several hundred MiB of memory for repeated or map fields, and more than 1 GiB of memory and more than 1 s of CPU for `Struct` or `ListValue` fields, in the worker process. The 4 MiB limit applies after decompression, so a proxy in front of a listener that faces untrusted clients must limit the decompressed request size.

## Configuration

`Extension::init(config)` receives everything. `rapira serve` resolves the `[grpc]` table of `rapira.toml` into this struct, then registers the extension.

| Field             | `rapira.toml` key                         | Meaning                                                                   |
| ----------------- | ----------------------------------------- | ------------------------------------------------------------------------- |
| `listen`          | `grpc.listen`                             | TCP address or unix socket path; default `127.0.0.1:50051`                |
| `schema`          | `grpc.descriptor_set`, `grpc.services`    | the loaded set (required) and the services to serve; default: the services of the files that no other file imports |
| `reflection`      | `grpc.reflection`                         | serve server reflection; default `false`                                  |
| `default_timeout` | `grpc.default_timeout_secs`               | timeout of a call that has none; unset: no deadline                       |
| `max_timeout`     | `grpc.max_timeout_secs`                   | upper limit for a client timeout; unset: no limit                         |
| `drain_grace`     | `supervisor.process_control_timeout_secs` | shutdown drain window; must expire before the host escalates its stop     |
| `keepalive_interval`, `keepalive_timeout` | none, 10 s each | HTTP/2 PING cadence and the wait for its ACK                    |

`descriptor_set` resolves against the directory of `rapira.toml`. `default_timeout_secs` must not be larger than `max_timeout_secs`. `[grpc.pool]` takes the keys of `[http.pool]`, and its `mode` must be `"dispatcher"`.

## Build

```sh
cargo build -p rapira_grpc
cargo clippy -p rapira_grpc --all-targets
```

The tests live in `crates/tests`: `tests/grpc_server.rs` and `tests/grpc_schema.rs` drive this crate over the wire against a fake PHP (`src/grpc.rs` is the harness), `tests/grpc_dispatcher.rs` and `tests/grpc_values.rs` cover the PHP side, and `tests/e2e/grpc.rs` runs the whole binary. `make grpc_fixtures` rebuilds the descriptor sets in `crates/tests/fixtures/grpc/` with a pinned `buf`.

## License

MIT, see [LICENSE](../../../LICENSE).
