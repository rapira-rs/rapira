## Settled, do not reopen

- NTS only, `rapira_sapi.h` rejects ZTS headers at compile time. Unix only.
- One interpreter per forked worker. Master is single-threaded, no tokio; workers inherit listener fds.
- MINIT runs once in the master pre-fork so opcache SHM is inherited. Workers exit rather than tear the module down.
- Foreground only, no daemonize. Pidfile stays.
- Allocator is mimalloc v3, without THP.
- New host logic in Rust via ZEND_API if that is reasonable. C only for ZPP shells, longjmp isolation, macro shims.
- Pre 1.0 - do not preserve backwards compatibility.

## PHP contract

- Here are the [rapira-rs/contract](https://github.com/rapira-rs/contract), local checkout in `../contract`. Read it before you plan, design or change anything PHP can see: stubs, classes, functions, exceptions, messages or behavior.
- Follow the contract. Ask before you change it or implement anything that differs from it.

## All text

- Use ASD-STE100 Simplified Technical English for all English text, including comments and docs.
- Use short sentences, active voice and approved vocabulary. Give one instruction per sentence.
- Use direct, literal language. Avoid idioms, slang, metaphors, decorative prose and unnecessary synonyms. Do not write poems in comments.

## Tests

- Test behavior through simple results a client or operator sees: responses, log records, exit statuses or scoreboard lines. Assert on these results, not internal counters, queues or join handles.
- Do not add public or private production API or branches only for tests. This includes functions, methods, constructors, accessors, `Default` implementations and feature flags. Do not add generics or traits with one production type and one test type. `#[cfg(test)]` and `pub(crate)` are not exceptions. Helpers inside `#[cfg(test)] mod tests` are test code.
- If no external path reaches a behavior, test its pure logic in a unit test or leave it untested. Do not add test hooks. Delete tests of sequences that production never runs.
- Unit tests belong in their crate's `#[cfg(test)] mod tests`. Test one small piece of code in its own scope. Private items are allowed. No fixtures: files, directories, sockets, child processes, signals, environment variables or PHP. Do not copy other rapira components as test doubles. In-memory inputs (futures, writers, wakers or paused tokio clocks) are not fixtures.
- Put tests that need fixtures in `crates/tests/tests/e2e/` behind the `e2e` feature so workspace runs skip them. Use `Spawn` in `harness.rs` to run the real `rapira` binary. Use PHP fixtures instead of fake plugins or PHP. Delete fixture tests that cannot be replaced by e2e tests.
- Reuse `crates/tests/src/`: `wire` (HTTP/1.1 client returning `Frame`s), `grpc` (gRPC, gRPC-Web and Connect clients), `server_log` (JSON log readers). Keep fixtures in `crates/tests/fixtures/` or `crates/tests/tests/e2e/fixtures/`. No `testing.rs`, `testdata/` or harness `[dev-dependencies]` in plugin crates. No tests in the root package's `tests/`.
- Do not assert that a port refuses connections after a stop: another process can bind the free port.
- New tests use worker or dispatcher mode, rarely classic.
- Check PHP behavior with php-src or a short script.
- Use Docker to test PHP minors older than the system PHP. Use the official `php:<minor>-cli-trixie` image at the digest in `.github/workflows/docker.yml`. It includes `libphp.so`. Add Rust from `rust:1-trixie`, as in `Dockerfile`. Install `clang`, `libclang-dev` and `procps` (`worker_pids` in `harness.rs` needs `ps`). Set `RUSTFLAGS="-L native=/usr/local/lib"` and `LD_LIBRARY_PATH=/usr/local/lib`. Use a target directory outside the checkout.
- The container runs PHP as root and lacks some CI tools. These differences can cause failures. Run a failing test on `main` in the same container before you report it.

`make test` (test_nts then test_e2e), `make test_nts`, `make test_e2e`, `make coverage`, `make stubs`. All derived from `php-config`, no hardcoded distro paths.

## Fuzz targets

- The libFuzzer targets in `crates/tests/tests/fuzz/` call parsers of client bytes in process. Each needs an oracle beyond "no panic", seeds in `seeds/<target>/` and a dictionary in `dict/<target>.dict`.
- Update fuzz targets in the same change as anything a client or PHP can see. Extend a target that reaches the changed code, or add one for new code that reads client bytes. Add new targets to the `long` matrix in `.github/workflows/fuzz.yml`. Add seeds and dictionary entries for new inputs.
- Call existing functions only. If needed, make an existing item `pub` or its module `pub mod`. Do not add functions, methods or types for a target.
- An oracle tolerates a known defect only with a one-line comment that links its issue. Remove the tolerance in the change that fixes the issue.
- Run each changed target for at least 60 s with its seeds and its dictionary (the command is in `CONTRIBUTING.md`). Break the changed code on purpose and check that the target fails before you commit.

## Dependencies

- Prefer `libc` directly over wrappers.
- Update dependencies and toolchains, including PHP in GitHub Actions and Rust, even with breaking changes. Update old versions instead of adding compatibility wrappers.

## Docs

- Pre-1.0: no migration framing, no old-to-new tables, no deprecation notes. Docs describe only the current design.

## Known false positives, do not "fix"

- rust-analyzer `E0277: Arguments<'_>: Sync` on `Box::pin` over a `tokio::select!`, while `cargo check` is clean. Cargo is authoritative.
- PHP 8.5 warns that `--enable-opcache` is unrecognized. The flag stays for the 8.4 CI leg.
- Extension visibility differs per CI leg; that is what the `extension_loaded` skip guards are for. Do not edit the test `php.ini`.
- `.clang-tidy` runs in survey mode, so Zend macro signatures trip `bugprone-*`. No CI job runs it.

## Design patterns

- Follow [Rust idioms and design patterns](https://rust-unofficial.github.io/patterns/).
