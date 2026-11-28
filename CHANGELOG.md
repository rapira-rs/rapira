# Changelog

## [0.10.0](https://github.com/rapira-rs/rapira/compare/v0.9.2...v0.10.0) (2026-11-28)


### ⚠ BREAKING CHANGES

* move from NTS worker processes to ZTS worker threads
* **config:** remove the io_threads key
* ship the ZTS PHP runtime
* build and test on ZTS PHP
* **sapi:** run each pool as a queue with fixed PHP worker threads
* **plugins:** share the IO runtime and drain each listener
* **net:** accept connections on the IO runtime
* run the server in one process
* **observability:** report in-process pool and worker state
* **config:** configure worker threads and the server stop deadline
* **scoreboard:** keep the worker state in the process
* **sapi:** keep putenv and the pool env in a per-thread table
* **sapi:** enter the entrypoint directory as php-src does
* **docker:** own PHP images from signed php-src tags, and the Rapira image on them
* **docker:** the PHP images are ghcr.io/rapira-rs/php:<minor>-nts and -zts. The rapira images keep the tags php<minor>, without a variant postfix, and are complete images, not a payload for COPY --from.
* **docker:** the image is a complete image with PHP, not a payload for COPY --from. Its tags end in -nts or -zts, for example php8.5-nts.
* **sapi,http,grpc:** less code between the sapi and the plugins
* **grpc:** the call object is its own ResponseMetadata

### Features

* **config:** add the env table of a pool ([fd3e6ee](https://github.com/rapira-rs/rapira/commit/fd3e6eed496719a1ceb019eed9516176db74eeac))
* **config:** configure worker threads and the server stop deadline ([ec69711](https://github.com/rapira-rs/rapira/commit/ec69711b7cdc818a9ca5a9982d885a9856197384))
* **docker:** build PHP from the signed php-src tag ([0e12d67](https://github.com/rapira-rs/rapira/commit/0e12d676412642235d95f7b2e187b941565b4a23))
* **docker:** own PHP images from signed php-src tags, and the Rapira image on them ([46b7e8e](https://github.com/rapira-rs/rapira/commit/46b7e8e143ac3bb2cc67134b66d1755ad6b35465))
* **grpc:** declare the Rest case of Call\Protocol ([9f40b2d](https://github.com/rapira-rs/rapira/commit/9f40b2d4046d2d05d98bd6aec7006cf6e916dcf2))
* **grpc:** declare the Rest case of Call\Protocol ([558fa2b](https://github.com/rapira-rs/rapira/commit/558fa2b842132f89fa8ed6c6b5c06fe9bf476d77))
* **grpc:** load the descriptor set from an HTTP or HTTPS URL ([ee18499](https://github.com/rapira-rs/rapira/commit/ee184993dc30130d42dd375fc47ef62af937bd5f))
* **grpc:** load the descriptor set from an HTTP or HTTPS URL ([f8f2cf6](https://github.com/rapira-rs/rapira/commit/f8f2cf64686974392a05e60ed25122c32ee2e6ef))
* **grpc:** serve server-streaming calls ([f56e8bc](https://github.com/rapira-rs/rapira/commit/f56e8bccf87860c5b0acaa2d73ba64800bd5908f))
* **grpc:** serve server-streaming calls ([cbc30b9](https://github.com/rapira-rs/rapira/commit/cbc30b99d91409e008269ddb6989aedefe0fa08e))
* move from NTS worker processes to ZTS worker threads ([4e23350](https://github.com/rapira-rs/rapira/commit/4e2335092143062ba81e262b6b1e99f25583f459))
* **net:** accept connections on the IO runtime ([d8a64bf](https://github.com/rapira-rs/rapira/commit/d8a64bf53c4b9ac46742d394bd50f1b5e6f82ba8))
* **observability:** report in-process pool and worker state ([37e888a](https://github.com/rapira-rs/rapira/commit/37e888a108113e823f274f141644adee518c2806))
* **plugins:** share the IO runtime and drain each listener ([65081c1](https://github.com/rapira-rs/rapira/commit/65081c1ab972153b46434ca79705993cc513a574))
* run the server in one process ([b372391](https://github.com/rapira-rs/rapira/commit/b3723911878f79f803e71e241d21a3bb7f498648))
* **sapi:** add count_objects() and get_objects() ([b88d0b2](https://github.com/rapira-rs/rapira/commit/b88d0b26d5b4fa2d99603dbd70ee634760246c57))
* **sapi:** add count_objects() and get_objects() ([eb88708](https://github.com/rapira-rs/rapira/commit/eb887088621c7b34d4628c88bea786edcd1773aa))
* **sapi:** add the execution_clock pool setting ([d1d7747](https://github.com/rapira-rs/rapira/commit/d1d77475714ebfa24b8efbdf480467cf3a4e8930))
* **sapi:** add the execution_clock pool setting ([10c3c1e](https://github.com/rapira-rs/rapira/commit/10c3c1e79c95a548fb5f07de8817e2a176e2ac74))
* **sapi:** add the stream park and the generator loop ([b50db44](https://github.com/rapira-rs/rapira/commit/b50db44fdff761c177439a351de0aef9ad84af10))
* **sapi:** coordinate OPcache resets and reload every pool ([9558151](https://github.com/rapira-rs/rapira/commit/955815125dc74a711920d036cd3f9dd7cc56ceda))
* **sapi:** each request $_SERVER gets the env pairs of the pool ([1c93926](https://github.com/rapira-rs/rapira/commit/1c93926a02556d7587754be57dd6ae7bc881dbd1))
* **sapi:** enter the entrypoint directory as php-src does ([92aaa87](https://github.com/rapira-rs/rapira/commit/92aaa871b66c28c16f51f7d1679b9c7a9166ee8a))
* **sapi:** failed boots back off and stop at max_boot_failures ([b14786b](https://github.com/rapira-rs/rapira/commit/b14786b00d560d1b47e9c337a051ba3b78b6b566))
* **sapi:** failed boots back off and stop at max_failures ([7759a01](https://github.com/rapira-rs/rapira/commit/7759a01ecd521528300940039d969ec82a779230))
* **sapi:** interrupt overdue units and enforce the watchdog grace period ([a567e7e](https://github.com/rapira-rs/rapira/commit/a567e7e772821a001a5b2edb76203ab315ac9d14))
* **sapi:** keep putenv and the pool env in a per-thread table ([f6de5b1](https://github.com/rapira-rs/rapira/commit/f6de5b1759304407aaf8ffe7b457f3e396d1c285))
* **sapi:** pause the POSIX execution timer during a park ([b735872](https://github.com/rapira-rs/rapira/commit/b73587205728bc8d094d94b6f43bef3bf6e1eb67))
* **sapi:** run each pool as a queue with fixed PHP worker threads ([527ffef](https://github.com/rapira-rs/rapira/commit/527ffef3645933b2b762fcb46e207ac1e23fd4cc))
* **sapi:** start one PHP engine with thread-local interpreter state ([82cebb0](https://github.com/rapira-rs/rapira/commit/82cebb073ca4de97761b2491ec4ac74f448617f2))
* **sapi:** start the boot retry wait at 500 ms ([a937bca](https://github.com/rapira-rs/rapira/commit/a937bca5d91e930e4d1ca5c1f2f5d935dada71ee))
* **sapi:** start TSRM and give the PHP thread its interpreter context ([5c2b521](https://github.com/rapira-rs/rapira/commit/5c2b521c6ad2f15cf842012e97d905d44dfd4498))
* **sapi:** workers set the env pairs of their pool ([b4c465f](https://github.com/rapira-rs/rapira/commit/b4c465f4a17a5f3893ae92761d77fad9c5bb9c6d))
* **scoreboard:** keep the worker state in the process ([92e7b1f](https://github.com/rapira-rs/rapira/commit/92e7b1f80fbf90c144fa72f9642322709c8e668b))
* set environment variables for the workers of a pool ([c82193b](https://github.com/rapira-rs/rapira/commit/c82193bcbab814ef2e947f53d7d8079e8e117b64))


### Bug Fixes

* **classic:** skip a queued unit whose client left ([9dbbfe5](https://github.com/rapira-rs/rapira/commit/9dbbfe51b0cb7d17937d6e6aebf60ea24b41f9ca))
* **grpc:** enforce the call deadline before accepting a reply ([5388f1f](https://github.com/rapira-rs/rapira/commit/5388f1fb2e67ddc3def2fbb849a1aa308d30422e))
* **grpc:** hold the deadline and the drain count for a whole stream ([5e6f987](https://github.com/rapira-rs/rapira/commit/5e6f9877494a8538bcb8210f497371e29dd7d25f))
* **http,grpc,sapi:** propagate shard failures, bound drain, and harden tests ([97b5abb](https://github.com/rapira-rs/rapira/commit/97b5abbd2b069af5b4a7a14e9d70fac632c27f06))
* **http,grpc:** write the layout asserts with static_assert ([dd3f6ef](https://github.com/rapira-rs/rapira/commit/dd3f6ef78c2c83f5b7e5c86b69a3b9cdf035639a))
* **http,master:** survive a lost local address and force the worker exit with _exit ([0478cef](https://github.com/rapira-rs/rapira/commit/0478cef4143882e4bf961110f45540e81069896a))
* **http,net,signals:** enforce Host, compression, truncation, accept, and signal checks ([48f22a4](https://github.com/rapira-rs/rapira/commit/48f22a450e38ab3d681a00ec9b6ab2a7c9f16f43))
* **http,sapi:** defer flush release, offload spool and encode, and sync docs ([9645472](https://github.com/rapira-rs/rapira/commit/96454723b1d25016447f53e1ba76c12e2b2747cb))
* **http:** drop a connection whose local address is not available ([c4d472c](https://github.com/rapira-rs/rapira/commit/c4d472c2ed8fc5a7e7d8306d85a8c346172f4b38))
* **http:** drop the sender after a push to a gone receiver ([20d02da](https://github.com/rapira-rs/rapira/commit/20d02da05cbad21f163013e853af296dfc2ef51f))
* **http:** keep the unused execution budget across a parked write ([9033cae](https://github.com/rapira-rs/rapira/commit/9033caeec677e0ada7f57d5e0aabb2c212de27b2))
* **http:** no lending after the quiesce of a cycle ([ce64786](https://github.com/rapira-rs/rapira/commit/ce64786c6ca47579d409f2c0db3e88592d5e4b02))
* **http:** release blocking threads between multipart frames ([15de506](https://github.com/rapira-rs/rapira/commit/15de5060c2b1cbcfe60d578507788546d4802ede))
* **http:** remove the old spool directories before the listener opens ([5b9cb02](https://github.com/rapira-rs/rapira/commit/5b9cb02fe2198ecd2f1b5a2aa64981c89c6ea940))
* **http:** remove the spool dir of a killed worker when its replacement starts ([3070ea1](https://github.com/rapira-rs/rapira/commit/3070ea116d4dff134771bfdbffe59744b2e76071))
* **http:** reserve only the body bytes in memory in the body budget ([5a2c66c](https://github.com/rapira-rs/rapira/commit/5a2c66cf50905e324fb72709c6bd89efb733294c))
* **http:** take the default server port from the bound listener ([a795e14](https://github.com/rapira-rs/rapira/commit/a795e143d54413b4c0bebfb0fcc82674ce654eb9))
* **http:** the sendFile root follows the app instance ([436face](https://github.com/rapira-rs/rapira/commit/436faceda1b3bd8e19868156de8b2ab719a92f59))
* **http:** the sendFile root has no state that accepts every path ([3ca0edc](https://github.com/rapira-rs/rapira/commit/3ca0edc4080765f96ab83264da0d7b7798b77366))
* **master:** end the worker with _exit on the second stop signal ([84fc370](https://github.com/rapira-rs/rapira/commit/84fc370deb3aa9ee6c51a966cc5cc162bcb96a16))
* **net:** drain accepted connections through their final response ([c4351a7](https://github.com/rapira-rs/rapira/commit/c4351a7f58b4d4e85cffdb90ec72272081936625))
* **net:** scope Linux IP imports and enable tokio rt feature ([241da6d](https://github.com/rapira-rs/rapira/commit/241da6d72377770d50c078654c824f5a392613df))
* **packages:** depend on the libssl.so.3 soname in the RPMs ([e1f1277](https://github.com/rapira-rs/rapira/commit/e1f127709a491334fc277039bf3acfd84c10ad0e))
* **packages:** require OpenSSL 3.3 and the RHEL 9.7 glibc in the PHP 8.6 packages ([2cb0cc5](https://github.com/rapira-rs/rapira/commit/2cb0cc5ed9132da4b55f75620aeeaedb385a0ad7))
* **packages:** require OpenSSL 3.3 and the RHEL 9.7 glibc in the PHP 8.6 packages ([2d7e7fb](https://github.com/rapira-rs/rapira/commit/2d7e7fbb02a2a30995cf31811fc44002a9b55c3d))
* RPM libssl.so.3 dependency, PUT body limit, and drain states ([6b800de](https://github.com/rapira-rs/rapira/commit/6b800de77b2bc76e52fe32ba2efce63de2f6d6ce))
* **sapi,http:** a reload moves each worker to the deployed release ([91d2cf5](https://github.com/rapira-rs/rapira/commit/91d2cf50f3b53ee4c4944da6a8720f591eb85d40))
* **sapi,http:** bound multipart buffers and tighten budget, reload, cleanup, and timer handling ([d84036e](https://github.com/rapira-rs/rapira/commit/d84036e40afe7e7c8a3399faa08f895f1adcf32b))
* **sapi:** a bailout in the OPcache state read stays in the helper ([c3ea17e](https://github.com/rapira-rs/rapira/commit/c3ea17e7cae27dd4de36e12b7197fc11f147d0b2))
* **sapi:** a failed OPcache state read ends a waiting app instance ([f89fea1](https://github.com/rapira-rs/rapira/commit/f89fea12ad34a60b9888c255c17ca4b43913e8f7))
* **sapi:** a reload starts with an empty OPcache ([63d561a](https://github.com/rapira-rs/rapira/commit/63d561aada21ddfc458a5df5ff19ce7014e792ad))
* **sapi:** a reload starts with an empty OPcache ([63bd722](https://github.com/rapira-rs/rapira/commit/63bd7226c0111e2f4e78cfe979e464c1cacbc181))
* **sapi:** a request shutdown clears the freed content type pointer ([0195b64](https://github.com/rapira-rs/rapira/commit/0195b640acebae1b2e5f93312a5a472c86086198))
* **sapi:** an app that boots after the OPcache reset request lends response pieces ([68c714f](https://github.com/rapira-rs/rapira/commit/68c714f5e30c5e96b187cb6a9e8f3b4b0714353b))
* **sapi:** an OPcache restart completes without a worker kill ([305a0e1](https://github.com/rapira-rs/rapira/commit/305a0e181a5ffb7a40edb4ec0402eb002d76f5cf))
* **sapi:** arm the full budget on the cpu execution clock ([76fc641](https://github.com/rapira-rs/rapira/commit/76fc641f95cfebdd277f0cbcff3b36b1e4aa3a35))
* **sapi:** call php_child_init() in each forked worker ([c4f9cc2](https://github.com/rapira-rs/rapira/commit/c4f9cc203f578f6c2046c712d2df8e1e9d765710))
* **sapi:** call php_child_init() in each forked worker ([921343a](https://github.com/rapira-rs/rapira/commit/921343ab715f10f111b16e1c1cce37a2d4474c9b))
* **sapi:** count the request body once in request_parse_body() ([577b891](https://github.com/rapira-rs/rapira/commit/577b891c39ad5243cdb603f59b28715597ab82c2))
* **sapi:** each app instance resolves the entrypoint and enters its directory ([71ad470](https://github.com/rapira-rs/rapira/commit/71ad470c3f35481bd9e19a4ca94c908160eb184b))
* **sapi:** every app restarts when OPcache schedules a restart ([af0b180](https://github.com/rapira-rs/rapira/commit/af0b180ee8757f5e41c3c8c8667e6576696558f2))
* **sapi:** exit only when every worker of a pool is blocked ([69218ff](https://github.com/rapira-rs/rapira/commit/69218ffaf46c6315637d9d214cf5d45502ab014a))
* **sapi:** fail the classic request when its OPcache reset request fails ([44e8b65](https://github.com/rapira-rs/rapira/commit/44e8b6579772f281766cfdb651f74f0fd2a1f6c3))
* **sapi:** give a child process of PHP an empty signal mask ([6eb0318](https://github.com/rapira-rs/rapira/commit/6eb0318a15c94bd8ff842952164807d9a8a8fa7c))
* **sapi:** give a teardown throw only the fatal record ([190bf79](https://github.com/rapira-rs/rapira/commit/190bf7909c11dee0d0e041eef9458647580d8994))
* **sapi:** keep the dispatcher idle through a blocking receive ([c686445](https://github.com/rapira-rs/rapira/commit/c6864456bf326d5a04eb9c4d289a0eacf5ca9633))
* **sapi:** keep the execution time limit the same across parks ([3cd3110](https://github.com/rapira-rs/rapira/commit/3cd31102ed1d5e5c741bedfdf041e8435f0ca641))
* **sapi:** keep the worker after a fatal in the header callback ([3e8ddd4](https://github.com/rapira-rs/rapira/commit/3e8ddd4eafb78fe7a91d57135e7ee7d15b4c75ee))
* **sapi:** leave queued work for ready workers during boot retries ([00edf0e](https://github.com/rapira-rs/rapira/commit/00edf0ef49afe65b536fcd4b86fd353fc056e7c1))
* **sapi:** mark the slot of a worker draining at the stop signal ([b7ecd00](https://github.com/rapira-rs/rapira/commit/b7ecd002826dfcfe61f2c5dcbb24aaa0c5956988))
* **sapi:** no OPcache state read after a memory_limit error ([d192e53](https://github.com/rapira-rs/rapira/commit/d192e530300cdb6a5e7c65d144930f6014be0bde))
* **sapi:** OPcache never kills a worker that holds its lock ([62dc11e](https://github.com/rapira-rs/rapira/commit/62dc11ebb37f988b0335f4bcedd4aaeb7d9031d9))
* **sapi:** read opcache.restrict_api with zend_ini_string() ([07edad4](https://github.com/rapira-rs/rapira/commit/07edad4d77f263282f7a95b31eec2c463a0207d6))
* **sapi:** read the pool environment for ${NAME} in INI text ([1e7b3ae](https://github.com/rapira-rs/rapira/commit/1e7b3ae1c41c3f0e15cbb15121c00ac2f7e65f06))
* **sapi:** reject PHP builds with ZEND_MAX_EXECUTION_TIMERS ([a4210e3](https://github.com/rapira-rs/rapira/commit/a4210e3029b9a6a1e4d2fdb61a24dbdeff339569))
* **sapi:** report a throw from a shutdown function in worker mode ([2967ce8](https://github.com/rapira-rs/rapira/commit/2967ce87d809540abe402ea473834e223052c176))
* **sapi:** report a throw from a shutdown function in worker mode ([fa42d7d](https://github.com/rapira-rs/rapira/commit/fa42d7d25375c4b921ca7884a6eac1b6ec0c6034))
* **sapi:** report a throw of the session write at the teardown ([df0f60e](https://github.com/rapira-rs/rapira/commit/df0f60e9e055a9bc01d2ca14dafb63d616f0f8ca))
* **sapi:** reserve SIGRTMAX for the request watchdog ([a6b0990](https://github.com/rapira-rs/rapira/commit/a6b0990387486fab18f8a1f111e0b39139600529))
* **sapi:** SCRIPT_NAME keeps the name of the configured entrypoint ([c7f0413](https://github.com/rapira-rs/rapira/commit/c7f0413f2c3f73b35b782c5b2817c37a2e95dfcc))
* **sapi:** stop the boots of a worker flagged unhealthy ([a9c55d8](https://github.com/rapira-rs/rapira/commit/a9c55d86d369d0131cfa001f478fa92a6854072c))
* **sapi:** wake parked response writers on a request timeout ([e0f55fb](https://github.com/rapira-rs/rapira/commit/e0f55fb36c5235521771de6bd750799b82445adf))
* **sapi:** write the session before the response leaves ([5d04be4](https://github.com/rapira-rs/rapira/commit/5d04be426ea1d758f230b74a70f30188c672a8f6))
* **sapi:** write the session before the response leaves ([bd8a7f5](https://github.com/rapira-rs/rapira/commit/bd8a7f5e09eed6760760a05884175f02a4f26e83))
* **sapi:** X-Powered-By reports the version of the loaded libphp ([93c0ee7](https://github.com/rapira-rs/rapira/commit/93c0ee763379b59a0cd47dedcbdef80ab3139729))
* **sapi:** X-Powered-By reports the version of the loaded libphp ([9390939](https://github.com/rapira-rs/rapira/commit/93909396b188683324c9446c1a1fb1fd8e8d3f5d))
* **scoreboard:** sample handled and failed totals from the same outcomes ([e329fc8](https://github.com/rapira-rs/rapira/commit/e329fc8675547922fdfd207131ff5a0c323552fe))
* spool cleanup, parked write budget and gone clients in classic mode ([25a3e18](https://github.com/rapira-rs/rapira/commit/25a3e1867051ab3c64c24c28bec255218053ff83))
* **static:** detect a file that a rename replaced with the same mtime and length ([cf83aa4](https://github.com/rapira-rs/rapira/commit/cf83aa4f1a421a60ec49182a7f7839cbdb76fa7c))
* **static:** detect a file that a rename replaced with the same mtime and length ([ad5893d](https://github.com/rapira-rs/rapira/commit/ad5893d0d191fa63ebdd393d576655b0a2809511))
* **tests:** copy the fixtures with a read and a write ([168f1c0](https://github.com/rapira-rs/rapira/commit/168f1c00ba60934d4abc1e45db1c3ca97b51c54c))


### Performance Improvements

* **http,sapi:** shard hyper LocalSet IO, add compress middleware, and simplify runtime ([8cc421e](https://github.com/rapira-rs/rapira/commit/8cc421eba1e2f56e322fc490ac2c7ed92c4771bd))
* **http:** parse a multipart body while it arrives ([9284fe9](https://github.com/rapira-rs/rapira/commit/9284fe994c78b23c4498a37bb18efc12d05d5519))
* **http:** parse multipart bodies from the transport chunks ([e9e408b](https://github.com/rapira-rs/rapira/commit/e9e408bf9c17348dec5c46a60c9641446e9f9cdf))
* **http:** parse multipart bodies from the transport chunks ([e9e408b](https://github.com/rapira-rs/rapira/commit/e9e408bf9c17348dec5c46a60c9641446e9f9cdf))
* **http:** parse multipart bodies from the transport chunks ([d0ff83d](https://github.com/rapira-rs/rapira/commit/d0ff83da8558825b9e16352011f4a5348bce0657))
* **http:** read a connection in pieces of at most 64 KiB ([1e225c3](https://github.com/rapira-rs/rapira/commit/1e225c36ec9750994e81cdfefcb1fad93d697ceb))
* **http:** the IO thread writes large writeBody pieces in place ([16f33df](https://github.com/rapira-rs/rapira/commit/16f33df3f2d6ed1af246dfcd9681ff7ef449ebb4))
* **http:** the IO thread writes large writeBody pieces in place ([a29cfb1](https://github.com/rapira-rs/rapira/commit/a29cfb1cf348d8bc67397437c9087e3a1153c176))
* **plugins:** release wire input after conversion ([568d1c3](https://github.com/rapira-rs/rapira/commit/568d1c3efe0826961344560910a584adb0ec1221))
* **sapi,http:** raw bodies travel as the transport chunks ([30d1ed0](https://github.com/rapira-rs/rapira/commit/30d1ed0b8f73bbfc47c189286689e640a9d4e342))
* **sapi,http:** raw bodies travel as the transport chunks ([30d1ed0](https://github.com/rapira-rs/rapira/commit/30d1ed0b8f73bbfc47c189286689e640a9d4e342))
* **sapi,http:** raw bodies travel as the transport chunks ([ac520eb](https://github.com/rapira-rs/rapira/commit/ac520eba967e16db7a308cacf7b543a1600b2160))
* **sapi:** a pull swaps the OPcache hook record only when it holds a reason ([eb1e8eb](https://github.com/rapira-rs/rapira/commit/eb1e8eb486e564bb6db4503e22a3a791f57e600d))
* **sapi:** build the constant $_SERVER entries once per worker ([5339ab0](https://github.com/rapira-rs/rapira/commit/5339ab00f6229a17b27f039003159180eb4f592b))
* **sapi:** build the constant $_SERVER entries once per worker ([a94fe73](https://github.com/rapira-rs/rapira/commit/a94fe73525e6e908380f253dab174e50bd8bbd39))
* **sapi:** hand PHP the raw body as a memory stream over one string ([6599aea](https://github.com/rapira-rs/rapira/commit/6599aea771c3f07ca943e3330690244975521908))
* **sapi:** share prepared CGI input with server variables ([f16d242](https://github.com/rapira-rs/rapira/commit/f16d24241b7a1fbfb5bce5581f1af604a7188aca))
* **sapi:** wake the worker that waited last for a queued unit ([ff0354d](https://github.com/rapira-rs/rapira/commit/ff0354d23afb6b7573f36d477987278e21e33434))
* **static:** compare file name suffixes without allocating ([7e19e7e](https://github.com/rapira-rs/rapira/commit/7e19e7ec1534d3bdaf880e2f2c74d5c5ef51143c))


### Reverts

* copy the fixtures with a read and a write ([4e533bb](https://github.com/rapira-rs/rapira/commit/4e533bb779ddb30bef3c0c6e2a09dedddd47880a))
* let Dependabot update the PHP base images ([2c60d5b](https://github.com/rapira-rs/rapira/commit/2c60d5b720911337fd442b56d428e3d9940fd569))
* link the php-src master arm64 job with LLD ([c864302](https://github.com/rapira-rs/rapira/commit/c864302108cf5258f85cf801e7e4cb313e617caf))


### Miscellaneous Chores

* start the next release after 0.9.2 ([2f0d193](https://github.com/rapira-rs/rapira/commit/2f0d1936877395f00da0c857391f983079d958f2))


### Code Refactoring

* **config:** remove the io_threads key ([e2f5383](https://github.com/rapira-rs/rapira/commit/e2f538387393bd2d459869af163cee92b8fb62d1))
* **docker:** build PHP and rapira in separate images ([bbae2e5](https://github.com/rapira-rs/rapira/commit/bbae2e5b37987d29d868d4d5e59b257f41590f9d))
* **grpc:** the call object is its own ResponseMetadata ([18e1bb7](https://github.com/rapira-rs/rapira/commit/18e1bb7b30406c80d4d2339d8754ba183809c35a))
* **sapi,http,grpc:** less code between the sapi and the plugins ([dfe9753](https://github.com/rapira-rs/rapira/commit/dfe9753f4bbeeb09e18b3128d7b1a99e45c4a9d5))


### Build System

* ship the ZTS PHP runtime ([7418591](https://github.com/rapira-rs/rapira/commit/7418591dae64f892a7c8ff9015b6604dfbef8f53))


### Continuous Integration

* build and test on ZTS PHP ([9ec2b77](https://github.com/rapira-rs/rapira/commit/9ec2b77f9b42e517a90349050185b7911cd30c48))

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
