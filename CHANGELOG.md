# Changelog

## [0.10.0](https://github.com/rapira-rs/rapira/compare/v0.9.2...v0.10.0) (2026-11-06)


### Bug Fixes

* **classic:** skip a queued unit whose client left ([9dbbfe5](https://github.com/rapira-rs/rapira/commit/9dbbfe51b0cb7d17937d6e6aebf60ea24b41f9ca))
* **http,grpc:** write the layout asserts with static_assert ([dd3f6ef](https://github.com/rapira-rs/rapira/commit/dd3f6ef78c2c83f5b7e5c86b69a3b9cdf035639a))
* **http:** keep the unused execution budget across a parked write ([9033cae](https://github.com/rapira-rs/rapira/commit/9033caeec677e0ada7f57d5e0aabb2c212de27b2))
* **http:** remove the spool dir of a killed worker when its replacement starts ([3070ea1](https://github.com/rapira-rs/rapira/commit/3070ea116d4dff134771bfdbffe59744b2e76071))
* **sapi:** reject PHP builds with ZEND_MAX_EXECUTION_TIMERS ([a4210e3](https://github.com/rapira-rs/rapira/commit/a4210e3029b9a6a1e4d2fdb61a24dbdeff339569))
* spool cleanup, parked write budget and gone clients in classic mode ([25a3e18](https://github.com/rapira-rs/rapira/commit/25a3e1867051ab3c64c24c28bec255218053ff83))


### Miscellaneous Chores

* start the next release after 0.9.2 ([2f0d193](https://github.com/rapira-rs/rapira/commit/2f0d1936877395f00da0c857391f983079d958f2))

## [0.9.2](https://github.com/rapira-rs/rapira/compare/v0.9.1...v0.9.2) (2026-11-01)

### ⚠️ Breaking Changes

- 🧹 **HTTP Configuration**: Removed `[http].unsafe_field_names`; configurations that contain it fail at startup. Classic and worker modes always drop header names that can conflict in the CGI variables, [#253](https://github.com/rapira-rs/rapira/pull/253).
- 🧹 **gRPC Protocol**: Removed `Rapira\Grpc\Call\Protocol::Rest`. The supported cases are `Grpc`, `GrpcWeb`, and `Connect`, [#253](https://github.com/rapira-rs/rapira/pull/253).
- 🧹 **PHP Body Memory**: Raw and URL-encoded request bodies now count toward PHP `memory_limit` in classic and worker modes. Check the limit for apps that accept large bodies, [#227](https://github.com/rapira-rs/rapira/pull/227).

### 🎯 Core

- ✨ **App Recycling**: App recycling preserves connections and queued requests at `max_requests` or on SIGHUP or SIGUSR2 to the master. Classic mode ignores these triggers, [#238](https://github.com/rapira-rs/rapira/pull/238), [#240](https://github.com/rapira-rs/rapira/pull/240).
- 🧹 **Reload Scope**: Reloads restart the apps with the current configuration and gRPC descriptors. Restart Rapira to load changed configuration or descriptor files, [#240](https://github.com/rapira-rs/rapira/pull/240).
- ✨ **Worker Memory Limits**: Added `max_memory_mb` to `[http.pool]` and `[grpc.pool]` as an RSS threshold in MiB. The master checks RSS once per second and replaces workers above the threshold after they drain, FR [#110](https://github.com/rapira-rs/rapira/issues/110).
- 🐛 **Request Failures**: PHP `max_execution_time` timeouts keep the worker process alive. In worker mode, a client disconnect ends the current request without recycling the app, [#225](https://github.com/rapira-rs/rapira/pull/225).
- 🐛 **Dispatcher Counts**: Cancelled units no longer count as active. Cancelled queued units do not consume `max_requests`, [#245](https://github.com/rapira-rs/rapira/pull/245).
- ⚡ **Request Processing**: Reduced body copies and waits, and added a budget for request bodies waiting for PHP. Idle workers release unused allocator memory after two seconds, [#226](https://github.com/rapira-rs/rapira/pull/226), [#227](https://github.com/rapira-rs/rapira/pull/227).

### 📦 `http` plugin

- ✨ **Server Identity**: Added `serverName()` and `serverPort()` to `HttpDispatcherInfo`. Both are available when the app starts, [#245](https://github.com/rapira-rs/rapira/pull/245).
- 🐛 **Connection Drain**: Stopping workers answer the next request on existing HTTP/1.1 connections with `Connection: close`, within the drain timeout, BUG [#236](https://github.com/rapira-rs/rapira/issues/236).
- 🐛 **Classic Responses**: Fixed incomplete streamed responses after `exit()` in exception handlers, including Yii2 error pages, BUG [#200](https://github.com/rapira-rs/rapira/issues/200).
- 🐛 **Uploads and File Responses**: Fixed upload spool retention, quoted multipart values, `sendFile()` length checks, and HEAD response lengths, [#226](https://github.com/rapira-rs/rapira/pull/226), [#251](https://github.com/rapira-rs/rapira/pull/251).
- ⚡ **Body and File Processing**: Multipart parsing uses a linear scan and sends bodies above 256 KiB to the blocking thread pool. `sendFile()` reads 256 KiB blocks, [#227](https://github.com/rapira-rs/rapira/pull/227), [#253](https://github.com/rapira-rs/rapira/pull/253).

### 📦 `grpc` plugin

- 🐛 **Connection Drain**: Connect and gRPC-Web calls on existing HTTP/1.1 connections receive a response before closure, within the drain timeout. HTTP/2 connections keep graceful GOAWAY, [#242](https://github.com/rapira-rs/rapira/pull/242).
- 🐛 **PHP Values**: `Status` and `Metadata` objects created without their constructors raise `ReflectionException` instead of crashing a worker, [#226](https://github.com/rapira-rs/rapira/pull/226).
- ⚡ **Response Compression**: gRPC and gRPC-Web responses use gzip only when the request used gzip. Connect responses can use gzip when the client accepts it, [#227](https://github.com/rapira-rs/rapira/pull/227).

### 📦 `static_files` middleware

- 🐛 **File Responses**: Fixed `If-Range` handling and serve the complete file for multi-range requests. Files with timestamps outside the HTTP date range remain available, [#226](https://github.com/rapira-rs/rapira/pull/226), [#251](https://github.com/rapira-rs/rapira/pull/251).

### 📦 PHP Packages

- ✨ **Sodium**: Added the Sodium extension to release builds and Docker images, FR [#220](https://github.com/rapira-rs/rapira/issues/220).
- 🐛 **Linux Archives**: Included the required shared libraries of `libphp.so`, BUG [#207](https://github.com/rapira-rs/rapira/issues/207).

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
