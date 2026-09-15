# Changelog

## [0.8.2](https://github.com/rapira-rs/rapira/compare/v0.8.1...v0.8.2) (2026-09-15)


### Bug Fixes

* **dispatcher:** allow receive after observed cancellation ([521d97b](https://github.com/rapira-rs/rapira/commit/521d97b85401a0f03329914fcc8498e86c9dd01e))
* **dispatcher:** log the discard of an abandoned exchange ([53a3615](https://github.com/rapira-rs/rapira/commit/53a3615aa8f507de238b42c97a571dcbda333804))
* **http:** arm the delivery watermark behind body-mapping middleware ([fef1338](https://github.com/rapira-rs/rapira/commit/fef133863b6d1144328a6402652b7c93a5ee3318))
* **http:** cancel PHP only when a completed response was never flushed ([1ffb8f5](https://github.com/rapira-rs/rapira/commit/1ffb8f51578ebd7da8fe555522363eeb10cf71c5))
* **http:** correct exchange completion and cancellation ([b84aa72](https://github.com/rapira-rs/rapira/commit/b84aa72d89c8ac4ed7d011cde269912bb3e789ce))
* **http:** frame no-body responses as empty for the delivery watermark ([a52cb77](https://github.com/rapira-rs/rapira/commit/a52cb775a477ea8a550183aee9618077284bd885))
* **http:** preserve exchanges after content-length completion ([4ff03e5](https://github.com/rapira-rs/rapira/commit/4ff03e5762eb1f3d9532294877b3064ec377a736))
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
