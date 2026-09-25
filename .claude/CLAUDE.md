## Settled, do not reopen

- NTS only, `wrapper.h` rejects ZTS headers at compile time. Unix only.
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

- `make stubs` generates each `crates/php_sys/*_arginfo.h` header from its `*.stub.php`.
- Joke comments (`Rustttt`, "trust me, I'm a developer") are intentional. Do not flag them.

## Tests

- Unit tests in-crate under `#[cfg(test)]`: only tests that need no fixture, no test double and no socket. Everything else is an integration test and goes to `crates/tests`: the test file under `crates/tests/tests/`, shared harness code (fakes, wire clients) under `crates/tests/src/`, fixtures and descriptor sets under `crates/tests/fixtures/`. No `testing.rs`, `testdata/` or harness `[dev-dependencies]` in a plugin crate. Never the root package's `tests/`.
- E2E lives in `crates/tests/tests/e2e/` behind the `e2e` feature, so a workspace run skips it.
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
