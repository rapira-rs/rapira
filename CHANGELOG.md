# Changelog

## [0.9.0](https://github.com/rapira-rs/rapira/compare/v0.8.1...v0.9.0) (2026-09-26)


### ⚠ BREAKING CHANGES

* remove the test-only API and run the tests on the rapira binary
* remove the test-only API and run the tests on the rapira binary
* **grpc:** the `Interceptor` type and the `interceptor` module leave the public API of rapira_grpc, and `[grpc].interceptors` now fails the boot as an unknown key.
* **sapi:** make the WorkerHooks callbacks required
* **sapi:** remove the class_exists and class_extends probes
* **sapi:** remove the channel route of Intake and Sink
* **sapi:** remove Rapira::start and its private scoreboard
* **grpc:** remove the test-only server and call API
* **http:** remove the test-only server API
* **net:** let Acceptor::adopt take the stop flag
* **sapi:** drop the entrypoint and mode fields from Worker
* **grpc:** let Plugin::modes be the one pool mode rule
* carry the drain grace in Worker and name the plugin in place of host and front
* **http,grpc:** run [http].middleware and [grpc].interceptors as tower layers
* **config:** let each plugin own its config section
* **grpc:** own the Rapira\Grpc PHP surface in the grpc plugin crate
* **http:** own the Rapira\Http PHP surface in the http plugin crate
* **build:** add rapira_php_build and register plugin classes through a MINIT registry
* **sapi:** replace Extension, ExtensionRuntime and Backend with Plugin, Work and Intake
* **sapi:** carry work units through one intake and set the dispatcher classes per worker
* **sapi:** fold extension_api and rapira_runtime into rapira_sapi and delete the mirror types
* **sapi:** rename php_sys to rapira_sapi and move the listener types to net
* **grpc:** extension_api::Backend has a new required method, unary(). \Rapira\Http\Tls is now \Rapira\Tls. The [http] table is optional.
* [pool] is now [http.pool]; --config, --processes, --mode, --listen, and the SCRIPT argument are removed.

### Features

* add pre-built PHP extensions ([6a3fb9f](https://github.com/rapira-rs/rapira/commit/6a3fb9fb512a47c9e82df1a5821a6d4dfdeed906))
* add pre-built PHP extensions ([677395d](https://github.com/rapira-rs/rapira/commit/677395d69ee499db139843c2ced4d5f03c1af54c))
* **build:** add rapira_php_build and register plugin classes through a MINIT registry ([545be18](https://github.com/rapira-rs/rapira/commit/545be185cfbc3e271462f89823196262032e5b3b))
* **grpc:** serve unary RPCs from PHP through ConnectRPC ([4f4d1b0](https://github.com/rapira-rs/rapira/commit/4f4d1b03a562fd7162f6e16289234edddc5cfdc9))
* **grpc:** serve unary RPCs from PHP through ConnectRPC ([#132](https://github.com/rapira-rs/rapira/issues/132)) ([fbaeb30](https://github.com/rapira-rs/rapira/commit/fbaeb30a1a8a3dc20bb7651037e967261f6d4db0))
* **http,grpc:** run [http].middleware and [grpc].interceptors as tower layers ([a15adb4](https://github.com/rapira-rs/rapira/commit/a15adb463501af82078a56c9fed64f1419f78963))
* worker pool per plugin ([aca6755](https://github.com/rapira-rs/rapira/commit/aca67556c47d44946d3415390818c674269afeb8))
* worker pool per plugin ([#104](https://github.com/rapira-rs/rapira/issues/104)) ([4778798](https://github.com/rapira-rs/rapira/commit/4778798b6e5f02f2d8a448a75e4e491b26a62139))


### Bug Fixes

* **dispatcher:** allow receive after observed cancellation ([521d97b](https://github.com/rapira-rs/rapira/commit/521d97b85401a0f03329914fcc8498e86c9dd01e))
* **dispatcher:** log the discard of an abandoned exchange ([53a3615](https://github.com/rapira-rs/rapira/commit/53a3615aa8f507de238b42c97a571dcbda333804))
* **grpc:** keep the status of a gRPC error in the trailers only ([2afbc11](https://github.com/rapira-rs/rapira/commit/2afbc11c4e48e90fd8c61c943c4f8a4549d95bbb))
* **grpc:** reject a descriptor set that lacks an imported file ([7add905](https://github.com/rapira-rs/rapira/commit/7add905f8f7d5b2b1ef72e84f5e98872c93f97ad))
* **http:** arm the delivery watermark behind body-mapping middleware ([fef1338](https://github.com/rapira-rs/rapira/commit/fef133863b6d1144328a6402652b7c93a5ee3318))
* **http:** cancel PHP only when a completed response was never flushed ([1ffb8f5](https://github.com/rapira-rs/rapira/commit/1ffb8f51578ebd7da8fe555522363eeb10cf71c5))
* **http:** cap the body reservation at max_body_size ([1cd460d](https://github.com/rapira-rs/rapira/commit/1cd460d3f1f92fcaf21151117ca9666f46ad18b0))
* **http:** correct exchange completion and cancellation ([b84aa72](https://github.com/rapira-rs/rapira/commit/b84aa72d89c8ac4ed7d011cde269912bb3e789ce))
* **http:** frame no-body responses as empty for the delivery watermark ([a52cb77](https://github.com/rapira-rs/rapira/commit/a52cb775a477ea8a550183aee9618077284bd885))
* **http:** preserve exchanges after content-length completion ([4ff03e5](https://github.com/rapira-rs/rapira/commit/4ff03e5762eb1f3d9532294877b3064ec377a736))
* **http:** rotate after a failed accept and end the loop on a failed rotation ([2580a4e](https://github.com/rapira-rs/rapira/commit/2580a4e32b1c968b8467a5ff3ca3299007330b61))
* **master:** keep an ondemand pool armed while it reloads ([1d35f8f](https://github.com/rapira-rs/rapira/commit/1d35f8fffdbe8712c3ebf718bf63daeac0518777))
* **master:** let workers drain when the master dies ([16b19d1](https://github.com/rapira-rs/rapira/commit/16b19d14d8dedecd927784272495a24ac73712c0))
* **master:** run the request watchdog while a pool reloads ([2974915](https://github.com/rapira-rs/rapira/commit/2974915bd301a1c5809c8e5bf0fac4f841ae6e11))
* probe the sendfile root at boot and pin the debug profile to one worker ([52bebef](https://github.com/rapira-rs/rapira/commit/52bebefc30dafa598e88aada335475acdcb00caf))
* **release:** bundle macOS dylibs that load through [@loader](https://github.com/loader)_path ([2fb1811](https://github.com/rapira-rs/rapira/commit/2fb1811e56d40a59f231a4c4507fd927cc50b48e))
* **release:** package OPcache for PHP 8.4 ([f8ba5b4](https://github.com/rapira-rs/rapira/commit/f8ba5b4d272439b2a8519f55fa01837fe8040777))
* **worker:** preserve retained SPL temporary streams ([2e28c19](https://github.com/rapira-rs/rapira/commit/2e28c19b7459fb65faf558ce44aea24f65a29462))
* **worker:** preserve retained SPL temporary streams ([b0e4ff6](https://github.com/rapira-rs/rapira/commit/b0e4ff63e9053c3ae8ea4cf1777d0ffa685fc256))


### Performance Improvements

* **api:** carry headers as HeaderMap end to end ([7a8fd31](https://github.com/rapira-rs/rapira/commit/7a8fd3150665e79fffb559a79ef62f9d7fc07e96))
* **classic:** drop the per-request entrypoint probe ([8626757](https://github.com/rapira-rs/rapira/commit/862675766e8966e84bdad0f902062e7e6b92b954))
* **http,grpc:** serve the empty interceptor chain unboxed and spawn connections without a box ([d23d0dd](https://github.com/rapira-rs/rapira/commit/d23d0ddc3fa31bb041680bf4a7182d4630751b2a))
* **http:** arm request timers only when the awaited future is pending ([70fdfd3](https://github.com/rapira-rs/rapira/commit/70fdfd33cfead6597331a8cb279969524bbd9805))
* **http:** block in epoll_wait so EPOLLEXCLUSIVE wakes one worker ([3c13465](https://github.com/rapira-rs/rapira/commit/3c134657857d639eaa7b5d7a165610fe9f191030))
* **http:** build the CGI view on the transport thread and serve every connection unboxed ([6ae917d](https://github.com/rapira-rs/rapira/commit/6ae917d38bf953ec01339cfa67dde5b01b2dad75))
* **http:** consume a queued End at body drop ([18d09d0](https://github.com/rapira-rs/rapira/commit/18d09d0043262bd1e6b7cbb3cf157b37b53d8c9f))
* **http:** consume a queued End before the drain task spawns ([0cdd977](https://github.com/rapira-rs/rapira/commit/0cdd977a9c3ccd3973adf4e1a93ab2abaeb86a9c))
* **http:** count flushes without a watch notification ([e8e1c15](https://github.com/rapira-rs/rapira/commit/e8e1c1567460289e451b59a7b0f54eda420f82c8))
* **http:** cut copies when the request is built ([0ea1180](https://github.com/rapira-rs/rapira/commit/0ea118060b06b69ad3e3bacad2d7559fb9060165))
* **http:** skip hop-by-hop removal when the response has no such field ([31c6c93](https://github.com/rapira-rs/rapira/commit/31c6c934dc7395302ea31271fff8f306f7a129ce))
* **http:** wake one worker per connection and trim the classic request path ([6dd6a1e](https://github.com/rapira-rs/rapira/commit/6dd6a1e5b7e1ce0c9363e07aa49ed62d40a9fd83))
* **http:** write getRequest() properties by slot and drop fmt from the view ([03f499e](https://github.com/rapira-rs/rapira/commit/03f499efe167934a3832bae34ed9b2ea6a736292))
* **php_sys:** build the Request view on the first getRequest() ([b663fb3](https://github.com/rapira-rs/rapira/commit/b663fb3f81518e6bdf33ac2483fcd8778c9b9778))
* **php_sys:** carry boxed jobs through the intake channel ([93cbb26](https://github.com/rapira-rs/rapira/commit/93cbb262092403fe7318e8809ea9b7c0b76e9faa))
* **php_sys:** drop per-request work on the response and log paths ([1966b42](https://github.com/rapira-rs/rapira/commit/1966b42ca37b26115f2829e78e0799573ffd1f99))
* **php_sys:** fold request headers in place for superglobals ([e14b673](https://github.com/rapira-rs/rapira/commit/e14b673f9da4b909eac01c8a22bf4c43868144ad))
* **php_sys:** hold the script paths once per worker ([4cc3acf](https://github.com/rapira-rs/rapira/commit/4cc3acf7909e1aa1a12c0a939157a68aa33e8262))
* **sapi:** append the digits of push_dec in one call ([5786159](https://github.com/rapira-rs/rapira/commit/5786159a1725a86130d554bdf1f76b65475f126a))
* **sapi:** register $_SERVER through php_register_known_variable ([00e4b17](https://github.com/rapira-rs/rapira/commit/00e4b17f763a00ddb0f973ff8dcba06bfd241db8))
* **sapi:** skip the timer disarm in receive() when a job is queued ([33b5c12](https://github.com/rapira-rs/rapira/commit/33b5c120d7420d554471c4c44edd4735de75ab13))
* **sapi:** wait for the plugin thread on a condvar ([a412c9f](https://github.com/rapira-rs/rapira/commit/a412c9f9bd008b512dd2398f6430fc62b3677bd2))
* trim the request path and drop dead code across the workspace ([a36a356](https://github.com/rapira-rs/rapira/commit/a36a356f8bbf9af5fb93c938636372de3c6f062e))
* **worker:** keep the working directory across classic requests ([ac1e9bc](https://github.com/rapira-rs/rapira/commit/ac1e9bc3e8f986da275243a1389cccf14cb69bc8))


### Code Refactoring

* carry the drain grace in Worker and name the plugin in place of host and front ([e0fe15b](https://github.com/rapira-rs/rapira/commit/e0fe15b1acf8239c308fcc5742aa165c7aca210b))
* **config:** let each plugin own its config section ([8bad96c](https://github.com/rapira-rs/rapira/commit/8bad96c8ade346b5dcba1a3645b366c501a4243e))
* **grpc:** let Plugin::modes be the one pool mode rule ([b1ef9c2](https://github.com/rapira-rs/rapira/commit/b1ef9c2ba8aab4bbeec7385100eb76aec264a0a6))
* **grpc:** own the Rapira\Grpc PHP surface in the grpc plugin crate ([36f44d6](https://github.com/rapira-rs/rapira/commit/36f44d6079b5f6fd62ecfe8133251fd8afeb8526))
* **grpc:** remove the interceptor path that no config can fill ([ba520bb](https://github.com/rapira-rs/rapira/commit/ba520bbf06df9fbaacd9f6e79afc2eda9ad232c7))
* **grpc:** remove the test-only server and call API ([ca86c42](https://github.com/rapira-rs/rapira/commit/ca86c42b3350564420500e7a82199856475fccb1))
* **http:** own the Rapira\Http PHP surface in the http plugin crate ([9d54d07](https://github.com/rapira-rs/rapira/commit/9d54d07ec0729efd817d24125c197e0afe54f883))
* **http:** remove the test-only server API ([c540f25](https://github.com/rapira-rs/rapira/commit/c540f25565ffb3e657ec68eb940a23f5e1c4755b))
* **net:** let Acceptor::adopt take the stop flag ([291c3f0](https://github.com/rapira-rs/rapira/commit/291c3f05ee7610f63ae7b6bf4e103866daf818cb))
* remove the test-only API and run the tests on the rapira binary ([4f0846e](https://github.com/rapira-rs/rapira/commit/4f0846e4057dbff190163312b6ddb82742f7a791))
* remove the test-only API and run the tests on the rapira binary ([4f0846e](https://github.com/rapira-rs/rapira/commit/4f0846e4057dbff190163312b6ddb82742f7a791))
* **sapi:** carry work units through one intake and set the dispatcher classes per worker ([be3dd06](https://github.com/rapira-rs/rapira/commit/be3dd068ebd949215710e6e71d19ce13ea6f9ddd))
* **sapi:** drop the entrypoint and mode fields from Worker ([86601a4](https://github.com/rapira-rs/rapira/commit/86601a497d56cdb3e3b25f407bf86e17e9f587b8))
* **sapi:** fold extension_api and rapira_runtime into rapira_sapi and delete the mirror types ([91cc60b](https://github.com/rapira-rs/rapira/commit/91cc60bcd39323a9f4dfb1e30097940c4c8ab59b))
* **sapi:** make the WorkerHooks callbacks required ([2d6d539](https://github.com/rapira-rs/rapira/commit/2d6d5392aed71340c540ef1c92068bf9cd2e2661))
* **sapi:** remove Rapira::start and its private scoreboard ([d786d23](https://github.com/rapira-rs/rapira/commit/d786d23eb08e40726a51ae85ec058ed29b725283))
* **sapi:** remove the channel route of Intake and Sink ([cd743e4](https://github.com/rapira-rs/rapira/commit/cd743e4ef53d99236b48e7e08bdb48b9345d02d8))
* **sapi:** remove the class_exists and class_extends probes ([e3c8810](https://github.com/rapira-rs/rapira/commit/e3c88100886e98123a61e37f461d32208c384ad3))
* **sapi:** rename php_sys to rapira_sapi and move the listener types to net ([88b7b77](https://github.com/rapira-rs/rapira/commit/88b7b77415047d29be5dacad1fd100d11059f957))
* **sapi:** replace Extension, ExtensionRuntime and Backend with Plugin, Work and Intake ([44c4ebe](https://github.com/rapira-rs/rapira/commit/44c4ebe7d3014fb95abead1e7be956da243ef394))

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
