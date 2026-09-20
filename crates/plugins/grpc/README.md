# gRPC plugin

The plugin serves native gRPC, binary gRPC-Web, and Connect on TCP or Unix sockets. Native gRPC uses cleartext HTTP/2. Binary gRPC-Web and Connect support HTTP/1.1 and HTTP/2 on the same listener. Each PHP worker handles one unary application call at a time. ConnectRPC handles protocol framing, compression, and error encoding. PHP receives and returns binary protobuf payloads.

Application calls use POST with `application/grpc+proto`, `application/grpc-web+proto`, or `application/proto`. Native gRPC and gRPC-Web also accept their content types without the `+proto` suffix. JSON application messages, gRPC-Web text encoding, and application streaming are unsupported.

## Configuration

```toml
[grpc]
listen = "127.0.0.1:9001"
protos = ["proto", "services/proto"]
import_paths = ["vendor/proto"]
reflection = true
interceptors = []
max_request_message_size_mb = 4
max_response_message_size_mb = 4

[grpc.compression.gzip]
enabled = false

[grpc.pool]
entrypoint = "grpc.php"
mode = "dispatcher"
processes = 4
```

Paths resolve against the configuration file. `protos` contains directories. Rapira searches them recursively for `.proto` files. Overlapping directories register each source file once. `import_paths` adds dependency roots. A service becomes an application service when its file is discovered under `protos`. Files loaded only through imports supply types and reflection data.

Import search roots use configuration order: `protos` first, then `import_paths`. Nested import roots retain their namespaces. Each import name must refer to one file. Discovered symlink file aliases cause a startup error; use one import name for each source file.

The master parses proto2 and proto3 schemas in Rust before workers fork. Standard `google/protobuf` imports are built in and resolve after the configured roots. Missing directories, invalid schemas, missing imports, conflicting definitions, and application streaming methods cause startup errors. Each method route is `/package.Service/Method`. Service names come from the schema.

Message limits apply to protobuf message bytes. Request limits also bound the encoded body or message, with an allowance for the five-byte frame prefix. The `_mb` values use MiB. Exceeding a limit produces `RESOURCE_EXHAUSTED`. The pool uses the same scaling, recycling, and process watchdog settings as an HTTP pool. HTTP and gRPC can run together with separate listeners and entrypoints.

Gzip response compression is disabled by default. Set `enabled = true` in `[grpc.compression.gzip]` to enable it for application responses. Native gRPC and gRPC-Web clients must advertise gzip in `grpc-accept-encoding`. Connect clients use `accept-encoding`; when this header is absent, Connect accepts the request's `content-encoding`. Gzip requests are accepted with either configuration setting. Decompression obeys the request message limit. Reflection responses use identity encoding.

Gzip can reduce bandwidth use for compressible payloads, but it uses CPU time. Keep response compression disabled when the CPU cost exceeds the bandwidth benefit, such as for random or already compressed payloads.

Schemas remain fixed for the master process. Worker replacement uses the prepared registry. Restart Rapira to load schema changes.

## PHP dispatch

`Rapira\get_dispatcher()` returns a `Rapira\Grpc\GrpcDispatcher`. Its `getServices()` method is available during application startup. Each service contains methods in descriptor order, with input and output message names.

`receive()` returns a `UnaryCall`. Use `getContext()` for method identity, request metadata, peer information, receipt time, and deadline. Use `getMessage()` for the protobuf payload. The application selects its generated PHP classes and decodes the payload.

- `respond(string $message)` completes the call with one protobuf response.
- `fail(Status $status)` completes the call with a gRPC error.
- `getResponseMetadata()` supplies the response header and trailer accumulator.
- `isCancelled()` reports host cancellation.
- `isFinalized()` reports completion.

The application must finalize its active call before it receives another. Repeated finalization throws `AlreadyFinalizedError`. A response after cancellation throws `WorkDiscardedException`. An abandoned call or an uncaught exception produces a sanitized `INTERNAL` status. Applications convert expected errors to `Status` and call `fail()`.

`Status` contains a `StatusCode`, a message, and `ErrorDetail` values. Each detail contains a protobuf type URL and serialized message bytes. Native gRPC and gRPC-Web encode these in `grpc-status-details-bin`. Connect uses an HTTP error status and a JSON error body. Its error details contain the protobuf message name and base64-encoded bytes.

## Metadata and deadlines

Metadata preserves repeated values. Names are case-insensitive for lookup. Stored names use lowercase. Values for names ending in `-bin` are raw bytes in PHP.

Add text values with `addHeader()` and `addTrailer()`. Add binary values with `addBinaryHeader()` and `addBinaryTrailer()`. Response metadata rejects transport-reserved names. Both halves become fixed when the call completes. `headers()` and `trailers()` return immutable snapshots.

Native gRPC sends trailers as HTTP/2 trailers. gRPC-Web puts trailers in the final body frame. Unary Connect sends trailers as response headers with the `trailer-` prefix.

The host enforces `grpc-timeout` for native gRPC and gRPC-Web, and `connect-timeout-ms` for Connect. The deadline starts at request receipt and includes interceptors, upload, queue waiting, and response waiting. The PHP context exposes the corresponding Unix deadline. Cancellation closes the waiting transport call. PHP can check cancellation while it runs. The process watchdog controls a stuck worker.

## Interceptors and reflection

The Rust plugin accepts the shared `Middleware` and `Next` API as its interceptor chain. All three wire protocols use this chain and carry `Protocol::Grpc` and `Peer` extensions. Interceptors can inspect requests, initial responses, and body trailers. A final gRPC status can arrive in trailers after HTTP status `200`.

The standalone binary accepts an empty `interceptors` list. Rust hosts can supply custom interceptors through `Config::interceptors`.

Reflection uses `grpc.reflection.v1` and `grpc.reflection.v1alpha`. Both run in Rust and use the same interceptor chain as application calls. Reflection lists registered application services and reflection services. It also serves imported descriptors and custom protobuf options. PHP's `getServices()` lists application services.

## Deployment

Deploy the application's `.proto` files with the PHP entrypoint. Configure schema dependencies with `import_paths`. Schema loading runs entirely in the Rapira process.

Generate PHP protobuf classes during the application build with the application's protobuf toolchain. See [the echo example](../../../examples/grpc/) for a complete server and reflection-based client commands.

Browser clients can call the binary gRPC-Web or Connect endpoint directly through a same-origin reverse proxy. Configure CORS at the reverse proxy for cross-origin calls. An Envoy `grpc_web` filter can also translate browser requests to native gRPC on this listener.

## Large payloads

Test connection count and concurrent calls separately. More concurrent large calls increase memory use and can reduce throughput. Measure CPU time per call and tail latency with the application's payloads.

Each PHP worker can retain one large request-string buffer until its worker cycle ends. This allocation counts toward PHP's `memory_limit`. Reuse requires all PHP references to the previous string to be released. Direct argument use permits reuse. For a persistent `$payload` variable, call `unset($payload)` after processing it. Reassignment evaluates the next `getMessage()` while the previous value is still live and requires another allocation.

A client's HTTP/2 receive-frame limit controls the maximum size of the server's response DATA frames. For large responses, test a 256 KiB limit. A Tonic client can set this with [`Endpoint::max_frame_size`](https://docs.rs/tonic/latest/tonic/transport/struct.Endpoint.html#method.max_frame_size):

```rust
let endpoint = tonic::transport::Endpoint::from_shared(uri)?
    .max_frame_size(256 * 1024u32);
```

The frame limit is independent of the protobuf message limit. Configure the client's message limit to accept the expected response size.
