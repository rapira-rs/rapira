# Contributing to Rapira

This repository contains the server: the SAPI core (`crates/php_sys`), the extension runtime, the pre-fork master and the `rapira` binary. The documentation site is a separate repository, [rapira-rs/rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io) - docs changes go there (see [contributing to the docs](https://rapira.rs/docs/contributing)).

## Prerequisites

- Rust stable - `rust-toolchain.toml` selects the exact channel for you
- A C compiler (the build compiles `crates/php_sys/*.c` against the PHP headers)
- libclang for bindgen (`libclang-dev` on Debian/Ubuntu, `clang-devel` on Fedora, `clang` on Arch)
- PHP 8.4 or 8.5, **NTS**, built with the embed SAPI (`--enable-embed=shared`). ZTS builds are rejected at compile time.

```sh
sudo apt install php8.4-dev libphp8.4-embed   # Debian/Ubuntu (deb.sury.org / ppa:ondrej)
sudo dnf install php-devel php-embedded       # Fedora/RHEL
sudo pacman -S php php-embed                  # Arch
sudo apk add php84-dev php84-embed            # Alpine
```

macOS notes and building PHP from source - including the exact configure line used for releases (`.github/php-configure-flags.txt`) - are covered in [build from source](https://rapira.rs/docs/build-from-source).

## Build

```sh
cargo build --release
```

PHP is discovered through `php-config`; point at a specific one with `PHP_CONFIG=/path/to/php-config`. Debian/Ubuntu ship only a versioned `libphpX.Y.so`, and a direct `cargo build` needs a plain `libphp.so` symlink next to it (`make test` and CI create that symlink themselves). At runtime the binary links `libphp.so` (`libphp.dylib` on macOS) dynamically; if it lives somewhere non-standard, set `LD_LIBRARY_PATH` (`DYLD_LIBRARY_PATH` on macOS).

## Tests

```sh
make test   # runs test_nts, then test_e2e - sequentially on purpose
```

- `make test_nts` - the in-process unit and integration suites (`cargo test --workspace`; the e2e suite is feature-gated off here).
- `make test_e2e` - the spawn-the-binary end-to-end suite (`crates/tests`, `--features e2e`): forks workers, binds ports, drives real HTTP, asserts signal/reload/scaling behavior. Single-threaded on purpose; never run it concurrently with `test_nts`.
- `make coverage` - needs `cargo install cargo-llvm-cov` and `rustup component add llvm-tools-preview`.
- `make stubs` - maintainers only: regenerates the three `crates/php_sys/*_arginfo.h` headers from the three `crates/php_sys/*.stub.php` stubs with PHP's `gen_stub.php`. Never edit the generated headers by hand.

Test placement: unit tests live inside their crate; integration tests in `crates/tests`; end-to-end tests under `crates/tests/tests/e2e/` behind the `e2e` feature.

## Lint and format

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p tests --features e2e --tests -- -D warnings
```

C sources (`crates/php_sys/*.c`, `*.h`) follow `.clang-format`.

## Repository layout

| Path                   | What it is                                                                          |
| ---------------------- | ----------------------------------------------------------------------------------- |
| `src/`                 | the `rapira` binary: CLI and boot                                                   |
| `crates/php_sys`       | the SAPI: C glue, bindgen bindings, worker/classic request loops, `rapira.stub.php` |
| `crates/runtime`       | the extension runtime that drives PHP                                               |
| `crates/master`        | the pre-fork supervisor: forking, reaping, scaling, signals, reload                 |
| `crates/config`        | `rapira.toml` configuration                                                         |
| `crates/api`           | the native extension contract                                                       |
| `crates/scoreboard`    | shared per-worker counters                                                          |
| `crates/plugins/http`  | the HTTP front                                                                      |
| `crates/middleware`    | built-in HTTP middleware, one crate per middleware                                  |
| `crates/tests`         | integration and e2e suites                                                          |

## Pull requests

Sign off your commits (`git commit -s`) and fill in the PR template. Bug reports and feature requests go through the [issue forms](https://github.com/rapira-rs/rapira/issues/new/choose); questions belong in [discussions](https://github.com/rapira-rs/rapira/discussions).

## Releases

Releases run through [release-please](https://github.com/googleapis/release-please). Every merge to `main` updates a release pull request with the next version and the changelog. When a maintainer merges that PR, one pipeline run tags the release, builds Linux x86_64/aarch64 and macOS aarch64 artifacts for PHP 8.4 and 8.5, builds the docker images, and publishes tarballs, `.deb`/`.rpm` packages and checksums to GitHub Releases. If a release run fails, re-run its failed jobs; the release stays a draft until the publish job completes. After a green CI run on `main`, a separate workflow re-points the `nightly-php*` docker tags and, except on the release commit, refreshes the rolling `nightly` prerelease with tarballs.

The version bump and the changelog come from the title of every commit that lands on `main`, so each title follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `feat:` for a minor bump, `fix:` for a patch, `!` or a `BREAKING CHANGE:` footer for a breaking change (a minor bump while the version is below 1.0.0). A squash merge collapses a PR into one commit, so its title is the one that counts. Put the issue reference at the end, `fix: register shutdown functions once (#84)`. A `[#84]:` prefix hides the commit from release-please, which needs the type at the start of the title.
