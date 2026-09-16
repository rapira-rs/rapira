# Changelog

## [0.9.0](https://github.com/rapira-rs/rapira/compare/v0.8.1...v0.9.0) (2026-09-16)


### ⚠ BREAKING CHANGES

* [pool] is now [http.pool]; --config, --processes, --mode, --listen, and the SCRIPT argument are removed.

### Features

* add pre-built PHP extensions ([6a3fb9f](https://github.com/rapira-rs/rapira/commit/6a3fb9fb512a47c9e82df1a5821a6d4dfdeed906))
* add pre-built PHP extensions ([677395d](https://github.com/rapira-rs/rapira/commit/677395d69ee499db139843c2ced4d5f03c1af54c))
* worker pool per plugin ([aca6755](https://github.com/rapira-rs/rapira/commit/aca67556c47d44946d3415390818c674269afeb8))
* worker pool per plugin ([#104](https://github.com/rapira-rs/rapira/issues/104)) ([4778798](https://github.com/rapira-rs/rapira/commit/4778798b6e5f02f2d8a448a75e4e491b26a62139))


### Bug Fixes

* **dispatcher:** allow receive after observed cancellation ([521d97b](https://github.com/rapira-rs/rapira/commit/521d97b85401a0f03329914fcc8498e86c9dd01e))
* **dispatcher:** log the discard of an abandoned exchange ([53a3615](https://github.com/rapira-rs/rapira/commit/53a3615aa8f507de238b42c97a571dcbda333804))
* **http:** arm the delivery watermark behind body-mapping middleware ([fef1338](https://github.com/rapira-rs/rapira/commit/fef133863b6d1144328a6402652b7c93a5ee3318))
* **http:** cancel PHP only when a completed response was never flushed ([1ffb8f5](https://github.com/rapira-rs/rapira/commit/1ffb8f51578ebd7da8fe555522363eeb10cf71c5))
* **http:** correct exchange completion and cancellation ([b84aa72](https://github.com/rapira-rs/rapira/commit/b84aa72d89c8ac4ed7d011cde269912bb3e789ce))
* **http:** frame no-body responses as empty for the delivery watermark ([a52cb77](https://github.com/rapira-rs/rapira/commit/a52cb775a477ea8a550183aee9618077284bd885))
* **http:** preserve exchanges after content-length completion ([4ff03e5](https://github.com/rapira-rs/rapira/commit/4ff03e5762eb1f3d9532294877b3064ec377a736))
* **master:** keep an ondemand pool armed while it reloads ([1d35f8f](https://github.com/rapira-rs/rapira/commit/1d35f8fffdbe8712c3ebf718bf63daeac0518777))
* **master:** run the request watchdog while a pool reloads ([2974915](https://github.com/rapira-rs/rapira/commit/2974915bd301a1c5809c8e5bf0fac4f841ae6e11))
* probe the sendfile root at boot and pin the debug profile to one worker ([52bebef](https://github.com/rapira-rs/rapira/commit/52bebefc30dafa598e88aada335475acdcb00caf))
* **release:** package OPcache for PHP 8.4 ([f8ba5b4](https://github.com/rapira-rs/rapira/commit/f8ba5b4d272439b2a8519f55fa01837fe8040777))
* **worker:** preserve retained SPL temporary streams ([2e28c19](https://github.com/rapira-rs/rapira/commit/2e28c19b7459fb65faf558ce44aea24f65a29462))
* **worker:** preserve retained SPL temporary streams ([b0e4ff6](https://github.com/rapira-rs/rapira/commit/b0e4ff63e9053c3ae8ea4cf1777d0ffa685fc256))

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
