## Settled, do not reopen

- NTS only, `rapira_sapi.h` rejects ZTS headers at compile time. Unix only.
- One interpreter per forked worker. Master is single-threaded, no tokio; workers inherit listener fds.
- MINIT runs once in the master pre-fork so opcache SHM is inherited. Workers exit rather than tear the module down.
- Foreground only, no daemonize. Pidfile stays.
- Allocator is mimalloc v3.
- New host logic in Rust via ZEND_API if that is reasonable. C only for ZPP shells, longjmp isolation, macro shims.
- Pre 1.0 - do not preserve backwards compatibility.

## PHP contract

- The PHP contract ([rapira-rs/contract](https://github.com/rapira-rs/contract), local checkout `../contract`; update it before you read it) comes first. Read it before you plan, design or change anything that PHP can see: stubs, classes, functions, exceptions, messages and behavior.
- The extension follows the contract. To improve the contract or deviate from it, ask first.

## Comments

- `make stubs` generates each `*_arginfo.h` header under `crates/` from the `*.stub.php` next to it.
- Joke comments (`Rustttt`, "trust me, I'm a developer") are intentional. Do not flag them.

## Tests

- A test proves a behavior from outside, through what a client or an operator sees. Its pass condition is simple: a response, a log record, an exit status or a scoreboard line.
- No public API for tests only: no function, constructor, accessor, `Default` impl or feature flag that only tests call. A `#[cfg(test)]` gate does not make one acceptable. No production branch that only a test path takes.
- If no path from outside reaches a behavior, do not add a hook for it. Test its pure logic in a unit test, or leave it without a test. Drop a test of a sequence that production never runs.
- Assert on the effect that a client sees, not on internal state such as a counter, a queue or a join handle.
- Unit tests stay in their crate under `#[cfg(test)]`. They can call private items. They use no fixture, no test double, no socket and no PHP.
- Every other test is an e2e test in `crates/tests/tests/e2e/` behind the `e2e` feature, so a workspace run skips it. It spawns the `rapira` binary with the `Spawn` builder in `harness.rs`. A stand-in plugin or a fake PHP is a test double: use a PHP fixture on the real binary.
- Shared client code is in `crates/tests/src/`: `wire` (an HTTP/1.1 client that returns `Frame`s), `grpc` (gRPC, gRPC-Web and Connect clients), `server_log` (the JSON log readers). Fixtures are in `crates/tests/fixtures/` and `crates/tests/tests/e2e/fixtures/`. No `testing.rs`, `testdata/` or harness `[dev-dependencies]` in a plugin crate. Never the root package's `tests/`.
- Do not assert that a port refuses connections after a stop: another process can bind the free port.
- New tests use worker or dispatcher mode, not classic.
- Check PHP behavior against php-src or a short script rather than guessing.

`make test` (test_nts then test_e2e), `make test_nts`, `make test_e2e`, `make coverage`, `make stubs`. All derived from `php-config`, no hardcoded distro paths.

## Dependencies

Prefer `libc` directly over wrappers.

## Docs

- Pre-1.0: no migration framing, no old-to-new tables, no deprecation notes. Docs describe only the current design.

## Known false positives, do not "fix"

- rust-analyzer `E0277: Arguments<'_>: Sync` on `Box::pin` over a `tokio::select!`, while `cargo check` is clean. Cargo is authoritative.
- PHP 8.5 warns that `--enable-opcache` is unrecognized. The flag stays for the 8.4 CI leg.
- Extension visibility differs per CI leg; that is what the `extension_loaded` skip guards are for. Do not edit the test `php.ini`.
- `.clang-tidy` runs in survey mode, so Zend macro signatures trip `bugprone-*`. No CI job runs it.
