## Settled, do not reopen

- NTS only, `build.rs` rejects ZTS. Unix only.
- One interpreter per forked worker. Master is single-threaded, no tokio; workers inherit listener fds.
- MINIT runs once in the master pre-fork so opcache SHM is inherited. Workers exit rather than tear the module down.
- Foreground only, no daemonize. Pidfile stays.
- Allocator is mimalloc v3.
- New host logic in Rust via ZEND_API if that is reasonable. C only for ZPP shells, longjmp isolation, macro shims.
- Pre 1.0 - do not preserve backwards compatibility.

## Comments

- `rapira_arginfo.h` is generated, regenerate from the stub.
- Joke comments (`Rustttt`, "trust me, I'm a developer") are intentional. Do not flag them.

## Tests

- Unit tests in-crate under `#[cfg(test)]`. Integration and e2e in `crates/tests`, never the root package's `tests/`.
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
