# Changelog

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
