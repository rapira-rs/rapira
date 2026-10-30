# Changelog

## [0.10.0](https://github.com/rapira-rs/rapira/compare/v0.9.1...v0.10.0) (2026-10-30)


### Features

* bundle the sodium PHP extension ([6a1a52a](https://github.com/rapira-rs/rapira/commit/6a1a52acad4adc0f69b22334e97c17d20b875e0f))
* bundle the sodium PHP extension ([e567f4a](https://github.com/rapira-rs/rapira/commit/e567f4a1b0c1e48dff6870f4318c1644ca5a2576)), closes [#220](https://github.com/rapira-rs/rapira/issues/220)
* **http:** expose server_name and server_port on HttpDispatcherInfo ([0a18793](https://github.com/rapira-rs/rapira/commit/0a1879311be015e6f9dd0670baafb4b5d055e4dd))
* **http:** expose server_name and server_port on HttpDispatcherInfo ([ca32802](https://github.com/rapira-rs/rapira/commit/ca328021dc6fdbfb467d2e6aa1710c64eca63808))
* **master:** replace a worker whose RSS goes above max_memory_mb ([5c3824c](https://github.com/rapira-rs/rapira/commit/5c3824cefabdd95cb7f3181977c27e83ec0fe63b))
* **master:** replace a worker whose RSS goes above max_memory_mb ([9d1e437](https://github.com/rapira-rs/rapira/commit/9d1e4370da90c97edadb592b87452b9879aafcfa)), closes [#110](https://github.com/rapira-rs/rapira/issues/110)
* restart the app inside the worker at max_requests ([2eff990](https://github.com/rapira-rs/rapira/commit/2eff9907b32e161479f693b4b05e2aef7f9ce9c6)), closes [#234](https://github.com/rapira-rs/rapira/issues/234)
* restart the app inside the worker on SIGUSR2 ([2f2c1cd](https://github.com/rapira-rs/rapira/commit/2f2c1cde3cde6758c9c71f5fff0bd411637d20f5)), closes [#235](https://github.com/rapira-rs/rapira/issues/235)
* restart the apps inside the workers on reload ([14dc95f](https://github.com/rapira-rs/rapira/commit/14dc95f0bc8f73b75579b610102dad0f12634112)), closes [#235](https://github.com/rapira-rs/rapira/issues/235)


### Bug Fixes

* classic mode speedup ([5670520](https://github.com/rapira-rs/rapira/commit/5670520f58fdfb4b83eddc29d474437dd9282dd8))
* classic mode speedup ([abae8d3](https://github.com/rapira-rs/rapira/commit/abae8d3a366a3631a09395fac1ef55ac45cc1f0a))
* **classic:** end the reply complete after exit() in a frameless callback ([f6d2744](https://github.com/rapira-rs/rapira/commit/f6d2744e60f2c9fe3c7c307a0148ad136954032a))
* **classic:** end the reply complete after exit() in a frameless callback ([876f3b3](https://github.com/rapira-rs/rapira/commit/876f3b3539d63235b2476fc5589c850cf30764e6)), closes [#200](https://github.com/rapira-rs/rapira/issues/200)
* **connection:** PHP app close does not touch the connection anymore ([4e68979](https://github.com/rapira-rs/rapira/commit/4e689796fb41e014ee0c39b7bf74445703691819))
* **connection:** PHP app close does not touch the connection anymore ([bb2b6ec](https://github.com/rapira-rs/rapira/commit/bb2b6ec501e0a4cf427c30cdecbad252ab0ac72d))
* copy the libraries of libphp.so into the Linux release ([db82240](https://github.com/rapira-rs/rapira/commit/db82240af52a57cd4170a095306f2d874e4e12a9))
* copy the libraries of libphp.so into the Linux release ([62206bf](https://github.com/rapira-rs/rapira/commit/62206bf04a9cbdffcabb98f7803e367aa194136a)), closes [#207](https://github.com/rapira-rs/rapira/issues/207)
* **grpc:** answer requests on held HTTP/1.1 connections during a worker drain ([46473ea](https://github.com/rapira-rs/rapira/commit/46473ea948f055f2b20b090efb4e8d352cee8da7)), closes [#237](https://github.com/rapira-rs/rapira/issues/237)
* **grpc:** refuse a Status or Metadata without its constructor ([f77b615](https://github.com/rapira-rs/rapira/commit/f77b615105358df4b3f36938eba84b0882cae6d1))
* **http,grpc:** answer 501 to a transfer coding other than one chunked ([845d3f7](https://github.com/rapira-rs/rapira/commit/845d3f7b80c01b0e91a0e5ba7a68e6c7776350bf))
* **http,sapi:** compare the HEAD method case-sensitively ([84c6cf1](https://github.com/rapira-rs/rapira/commit/84c6cf1d8e31b41e109007308463e2bb57d174b6))
* **http,static:** open files without blocking on a FIFO ([a5f938d](https://github.com/rapira-rs/rapira/commit/a5f938db15a357a70403bf090614e0b85885edd5))
* **http:** accept int keys in writeHead() and writeTrailers() ([b3a73cf](https://github.com/rapira-rs/rapira/commit/b3a73cf5b9999331c9a88f13f87625e1a2ea5004))
* **http:** answer 400 to a Host value that is not uri-host [":" port] ([e03295c](https://github.com/rapira-rs/rapira/commit/e03295c0faaa6173024041a863ac93209ccd3165))
* **http:** answer requests on held connections during a worker drain ([4e68979](https://github.com/rapira-rs/rapira/commit/4e689796fb41e014ee0c39b7bf74445703691819))
* **http:** answer requests on held connections during a worker drain ([3a238d7](https://github.com/rapira-rs/rapira/commit/3a238d77a044b0a8966304cfbcbf4967321fc0f7)), closes [#236](https://github.com/rapira-rs/rapira/issues/236)
* **http:** keep backslashes in quoted content-disposition values ([fac1cf0](https://github.com/rapira-rs/rapira/commit/fac1cf0110eec9895578deabac59dbb5b991b0d3))
* **http:** keep upload spools, sendFile limits and HEAD lengths correct ([5bf2465](https://github.com/rapira-rs/rapira/commit/5bf246590fddd2fa08c1898baa5aee96c787ca8d))
* **http:** set the server identity before the fork ([2766cba](https://github.com/rapira-rs/rapira/commit/2766cba06b594d8ab651e093c12822d9ec45c18c))
* limit the request body bytes that wait for a worker ([7c2f24a](https://github.com/rapira-rs/rapira/commit/7c2f24a3f8865df983890916b751bb278728fcd8))
* **master:** keep workers alive on USR signals and close listeners at stop ([fc27c86](https://github.com/rapira-rs/rapira/commit/fc27c860214198d7b7499f8e04d1922a0fe04c58))
* resolve the remaining fuzzing findings in the HTTP, static file and log paths ([db277b7](https://github.com/rapira-rs/rapira/commit/db277b7355d837c2f4b78be1324c53e1a79d71ab))
* **sapi,http,grpc:** fix contract defects at the plugin bridge ([c415624](https://github.com/rapira-rs/rapira/commit/c415624df5d2143b7b7591b8f188459c9e9a816d))
* **sapi,http,grpc:** use the contract texts in the dispatcher errors ([a2303ba](https://github.com/rapira-rs/rapira/commit/a2303ba5e1d55a9e138f47d4c67038085444bdcb))
* **sapi:** a cancelled queued unit does not use the max_requests quota ([5f2631b](https://github.com/rapira-rs/rapira/commit/5f2631bb8a9174829d0c86ff22f9c20ed13c4b0b))
* **sapi:** activeCount() does not count a cancelled unit ([6283176](https://github.com/rapira-rs/rapira/commit/6283176be359044e637b8495fb46e835272f8ede))
* **sapi:** drop a stale dispatcher after a shutdown bailout ([6b8a5e1](https://github.com/rapira-rs/rapira/commit/6b8a5e1df949545b75667479010648897aa432f9))
* **sapi:** end only the unit when a worker-mode client leaves ([db5cf45](https://github.com/rapira-rs/rapira/commit/db5cf452fa8017ee9620ddb5f8c072ae9dac4c13))
* **sapi:** keep request-path bailouts inside zend_try ([a1f95df](https://github.com/rapira-rs/rapira/commit/a1f95df8903b20266c6855abb10ea82173a3effd))
* **sapi:** keep the worker process when max_execution_time expires ([28bed2b](https://github.com/rapira-rs/rapira/commit/28bed2b5f0be3822fac6f0e098073418e0eb220b))
* **sapi:** release a computed Throwable property in Rapira\log ([645a43e](https://github.com/rapira-rs/rapira/commit/645a43ec2a37fd4a2eab8508e2fe2d3c764aab57))
* **sapi:** send the reload as an intake entry, not as a unit ([f491c40](https://github.com/rapira-rs/rapira/commit/f491c40a87bd1002621f2a04c87a981e2f7815f2))
* seal a teardown flush bailout as an error ([f7199ca](https://github.com/rapira-rs/rapira/commit/f7199ca5cd76d8fc1d9d03c00efd923b372752ef))
* **static:** honour If-Range and answer multi-range requests with the file ([cb2ded6](https://github.com/rapira-rs/rapira/commit/cb2ded69d6420ff44d3d8b93dc23b8d21847d250))
* **static:** serve a file with an mtime outside 1970..9999 ([65e7f99](https://github.com/rapira-rs/rapira/commit/65e7f992dd4f3cc9bb53964e1ea4fea3733c8986))


### Performance Improvements

* **grpc:** compress Connect replies and gRPC replies to gzip requests ([b0f45dc](https://github.com/rapira-rs/rapira/commit/b0f45dcb92927841de212ce60cbca60cfed0e05f))
* **grpc:** send gRPC replies without compression ([c267e13](https://github.com/rapira-rs/rapira/commit/c267e13e32deb77621629f88e924ea832f5b689d))
* **http:** parse multipart bodies on the IO thread ([060f77e](https://github.com/rapira-rs/rapira/commit/060f77efb85a07ae0ce9227e44ef84632ea64901))
* **http:** parse multipart bodies over 256 KiB on the blocking pool ([b0a9073](https://github.com/rapira-rs/rapira/commit/b0a9073981e27d600c2e004fd1bfb40a4c88edeb))
* **http:** raise the reply frame cap from 4 to 16 ([2689c37](https://github.com/rapira-rs/rapira/commit/2689c37da321013329b0c23cdba390b60657f2a8))
* **http:** read sendFile() in 256 KiB slices ([eac0ae0](https://github.com/rapira-rs/rapira/commit/eac0ae0e1df273009a95a3e976e2dc33e7354eae))
* **sapi:** hand PHP the buffered body as one memory stream ([5e5d77a](https://github.com/rapira-rs/rapira/commit/5e5d77a8b48b2964a16b5289b5739c41a94e1dd8))
* **sapi:** purge freed mimalloc pages after a 2 s idle park ([5c0fecb](https://github.com/rapira-rs/rapira/commit/5c0fecba30d0e8531d31aa5f5e6530a2f9454681))
* **sapi:** wait for an intake slot without polling ([e5a80d4](https://github.com/rapira-rs/rapira/commit/e5a80d442f513770f6f41f77a48207dc789d8e6f))
* **sapi:** write a PHP diagnostic once when php_error_cb logged it ([02e6131](https://github.com/rapira-rs/rapira/commit/02e61311d40684eea7a9673fa0c25bacddb2d1e9))


### Reverts

* drop the Host, transfer coding, FIFO and int header key fixes ([41f55be](https://github.com/rapira-rs/rapira/commit/41f55bec58f00a30666114a859f0d36f01a8f9f5))

## [0.9.1](https://github.com/rapira-rs/rapira/compare/v0.9.0...v0.9.1) (2026-10-07)

### 🎯 Core

- 🐛 **Work State Checks**: Fixed worker crashes after `isCancelled()` or `isFinalized()` calls in x86_64 release builds. The builds use Rust 1.98.1 until a Rust release fixes the compiler defect, BUG [#201](https://github.com/rapira-rs/rapira/issues/201) (thanks @roxblnfk).

### 📦 `http` plugin

- 🐛 **HTTP/1.1 Headers**: Responses with `Transfer-Encoding` no longer send `Content-Length`. A `close` token in any `Connection` request header closes the connection, even when a later one says `keep-alive`, [#202](https://github.com/rapira-rs/rapira/pull/202).

### 📦 PHP Packages

- 🧹 **Docker Images**: Updated the PHP base images to 8.5.11 and 8.4.26, [#202](https://github.com/rapira-rs/rapira/pull/202).
- 🧹 **macOS Archives**: The macOS archive requires macOS 26 or later. It bundles the Homebrew libraries of the `macos-latest` build runner, [#202](https://github.com/rapira-rs/rapira/pull/202).

## [0.9.0](https://github.com/rapira-rs/rapira/compare/v0.8.1...v0.9.0) (2026-10-05)

### 🎯 Core

- ✨ **Plugin Worker Pools**: Added independent worker pools under `[http.pool]` and `[grpc.pool]`, FR [#104](https://github.com/rapira-rs/rapira/issues/104).
- 🧹 **Pool Configuration**: Each pool runs a fixed `processes` count. The server command is `rapira serve <CONFIG>`, [#149](https://github.com/rapira-rs/rapira/issues/149), [#116](https://github.com/rapira-rs/rapira/pull/116).
- ✨ **Worker Boot Environment**: Added process environment values, script paths, `argv`, and `argc` to `$_SERVER` at worker and dispatcher boot, FR [#129](https://github.com/rapira-rs/rapira/issues/129) (thanks @FluffyDiscord).
- 🧹 **Plugin API**: Consolidated the PHP runtime in `rapira_sapi` and simplified the Rust plugin API. Each plugin owns its PHP classes and configuration, [#144](https://github.com/rapira-rs/rapira/pull/144), [#145](https://github.com/rapira-rs/rapira/pull/145), [#194](https://github.com/rapira-rs/rapira/pull/194).
- 🐛 **Worker Memory**: Disabled transparent huge pages on Linux to reduce worker memory use, [#151](https://github.com/rapira-rs/rapira/issues/151).
- 🐛 **Temporary Streams**: Kept retained `SplTempFileObject` streams usable across worker requests, [#111](https://github.com/rapira-rs/rapira/pull/111).
- 🐛 **Worker Lifecycle**: Workers can drain active requests after the master exits unexpectedly. The request watchdog stays active during pool reloads, [#130](https://github.com/rapira-rs/rapira/pull/130), [#116](https://github.com/rapira-rs/rapira/pull/116).

### 📦 `http` plugin

- ⚡ **Request Performance**: Reduced request copies, allocations, and system calls. On Linux, one waiting worker wakes for each new connection, [#127](https://github.com/rapira-rs/rapira/pull/127), [#130](https://github.com/rapira-rs/rapira/pull/130).
- 🧹 **Classic Working Directory**: Classic workers keep their working directory across requests, so a `chdir()` applies until the worker exits, [#127](https://github.com/rapira-rs/rapira/pull/127).
- 🐛 **Exchange Completion**: Fixed false cancellation after a complete response. Dispatchers can accept new work after client cancellation, [#113](https://github.com/rapira-rs/rapira/pull/113).

### 📦 `grpc` plugin

- ✨ **Unary RPCs**: Added PHP handlers for gRPC, binary gRPC-Web, and Connect with protobuf or JSON messages. Includes health checks and optional server reflection, FR [#132](https://github.com/rapira-rs/rapira/issues/132).
- ✨ **Authentication**: Added interceptor chains and a built-in bearer-token check before calls reach PHP, FR [#36](https://github.com/rapira-rs/rapira/issues/36).
- ✨ **HTTP/2 Keepalive**: Added `keepalive_interval_secs` and `keepalive_timeout_secs` to `[grpc]`, [#148](https://github.com/rapira-rs/rapira/pull/148).

### 📊 Observability

- ✨ **Prometheus Metrics**: Added `/metrics` for worker state, requests, queues, exits, and Linux memory use. Counters persist across worker respawns, FR [#83](https://github.com/rapira-rs/rapira/issues/83) (thanks @Zylius).
- ✨ **Health Probes**: Added `/livez` and `/readyz` for master liveness and PHP pool readiness, FR [#49](https://github.com/rapira-rs/rapira/issues/49) (thanks @rauanmayemir).
- 🧹 **Log Timestamps**: JSON logs include microseconds in `timestamp`, [#194](https://github.com/rapira-rs/rapira/pull/194).

### 📦 PHP Packages

- ✨ **Pre-built Extensions**: Added `pdo_pgsql`, `pgsql`, `bcmath`, `intl`, `igbinary`, and `redis` to release builds and Docker images, FR [#103](https://github.com/rapira-rs/rapira/issues/103) (thanks @FluffyDiscord).
- 🐛 **Release Bundles**: Included OPcache in PHP 8.4 packages and required shared libraries in macOS archives, [#117](https://github.com/rapira-rs/rapira/pull/117), [#130](https://github.com/rapira-rs/rapira/pull/130).
- 🐛 **PHP 8.7 Builds**: Fixed builds against PHP 8.7 development headers, [#156](https://github.com/rapira-rs/rapira/pull/156).

## [0.8.1](https://github.com/rapira-rs/rapira/compare/v0.8.0...v0.8.1) (2026-09-05)

### 🎯 Core

- 🐛 **Connection Distribution**: Improved distribution of new HTTP connections across workers on Linux for TCP and Unix sockets, BUG [#107](https://github.com/rapira-rs/rapira/issues/107) (thanks @FluffyDiscord).

## [0.8.0](https://github.com/rapira-rs/rapira/compare/v0.7.0...v0.8.0) (2026-09-02)

### 🎯 Core

- ✨ **Hyper HTTP Server**: Replaced the Pingora HTTP layer with a Hyper and Tower HTTP/1.1 server. Added a shared middleware interface, FR [#35](https://github.com/rapira-rs/rapira/issues/35).
- ✨ **Runtime Mode API**: Added `\Rapira\Mode` and `\Rapira\get_mode()` for Classic, Worker, and Dispatcher modes. Renamed `NotInWorkerModeError` to `NoDispatcherError`, FR [#77](https://github.com/rapira-rs/rapira/issues/77) (thanks @roxblnfk).
- ✨ **PHP Handler Lifetime**: Built one `PHPHandler` for each connection and separated per-request state, FR [#99](https://github.com/rapira-rs/rapira/issues/99).
- 🐛 **Worker Shutdown**: Stopped the per-request destructor sweep. Boot shutdown functions now run once when the worker exits. Long-lived boot objects remain usable across requests, BUG [#82](https://github.com/rapira-rs/rapira/issues/82) (thanks @Zylius).

### 📦 `static_files` middleware

- ✨ **Static File Serving**: Added configurable static file serving. File misses continue to PHP, FR [#76](https://github.com/rapira-rs/rapira/issues/76).
- ✨ **Static File Cache**: Added a per-worker memory cache with one-second revalidation, a 16 MiB capacity, and a 256 KiB limit for each file, FR [#98](https://github.com/rapira-rs/rapira/issues/98).
