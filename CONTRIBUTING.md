# Contributing to Rapira

This repository contains the server: the SAPI crate (`crates/sapi`), the plugins, the pre-fork master and the `rapira` binary. The documentation site is a separate repository, [rapira-rs/rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io) - docs changes go there (see [contributing to the docs](https://rapira.rs/docs/contributing)).

## Prerequisites

- Rust stable - `rust-toolchain.toml` selects the exact channel for you
- A C compiler (the build compiles `crates/sapi/*.c` and the C method shells of each plugin against the PHP headers)
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
- `make stubs` - maintainers only: regenerates each `*_arginfo.h` header under `crates/` from the `*.stub.php` stub next to it with PHP's `gen_stub.php`. Never edit the generated headers by hand.

Test placement: unit tests live inside their crate; integration tests, their harness (`crates/tests/src/`) and their fixtures (`crates/tests/fixtures/`) in `crates/tests`; end-to-end tests under `crates/tests/tests/e2e/` behind the `e2e` feature.

## Lint and format

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p tests --features e2e --tests -- -D warnings
```

C sources (`*.c`, `*.h` under `crates/`) follow `.clang-format`.

## Repository layout

| Crate                 | Directory                        | Role                                                                                                                                         |
| --------------------- | -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `rapira_core`         | `src/`                           | the `rapira` binary: CLI, the `rapira.toml` file shape, the plugin list, boot, fork and the worker entry                                     |
| `rapira_php_build`    | `crates/php_build`               | build helper: `php-config` discovery and the C compile of the method shells, for `rapira_sapi` and every plugin with a PHP surface           |
| `rapira_net`          | `crates/net`                     | listeners: `ListenAddr`, `PrepareCtx` and `PreparedListener` bind in the master before the fork, `Acceptor` adopts and accepts in the worker |
| `rapira_sapi`         | `crates/sapi`                    | the embed SAPI: boot, the PHP thread, the intake, the base PHP contract, the classic and worker modes, the `Plugin` and `Work` traits        |
| `rapira_config`       | `crates/config`                  | the shared config shapes: `[supervisor]`, `[log]`, the pool table, `listen`, the duration and path helpers                                   |
| `rapira_master`       | `crates/master`                  | the pre-fork supervisor: forking, reaping, scaling, signals, reload                                                                          |
| `rapira_scoreboard`   | `crates/scoreboard`              | shared per-worker counters                                                                                                                   |
| `rapira_http`         | `crates/plugins/http`            | the http plugin: HTTP/1.1, the `Rapira\Http` classes, the `[http]` table ([README](crates/plugins/http/README.md))                           |
| `rapira_grpc`         | `crates/plugins/grpc`            | the grpc plugin: gRPC, gRPC-Web and Connect, the `Rapira\Grpc` classes, the `[grpc]` table ([README](crates/plugins/grpc/README.md))         |
| `rapira_static_files` | `crates/middleware/static_files` | the `static` http middleware, a tower layer; each built-in http middleware is one crate under `crates/middleware`                            |
| `tests`               | `crates/tests`                   | integration and e2e suites                                                                                                                   |

## Plugins

- A dispatcher plugin owns a pool and turns each request into a work unit that PHP pulls with `receive()`: the http plugin makes a `Rapira\Http\Exchange`, the grpc plugin makes a `Rapira\Grpc\UnaryCall`.
- A capability plugin owns no pool: a pool enables it, each worker of that pool starts it after the fork, and PHP gets it through the second acquisition path of the contract. No capability plugin exists.
- A middleware (http) or an interceptor (grpc) is a tower layer that one plugin applies around its inner service in config order. It never touches PHP.

A plugin is one crate under `crates/plugins` that implements `rapira_sapi::plugin::Plugin`. It owns its config table, its PHP stub, its C method shells, the Rust behind those methods, its work unit and its transport. The two plugin READMEs describe the crate layout.

To add a plugin:

- Create its crate under `crates/plugins`.
- In the root `Cargo.toml`, add the crate to the workspace `members`, to `[workspace.dependencies]` and to the `[dependencies]` of `rapira_core`.
- In `src/settings.rs`, add its table to `FileConfig` and its settings to `Settings`.
- In `settings` in `src/settings.rs`, call its `resolve`, and add its table to the check that refuses a file with no plugin table.
- In `serve` in `src/main.rs`, build the plugin with its pool, and add its `PhpPart` to the `boot_master` call.

## Pull requests

Sign off your commits (`git commit -s`) and fill in the PR template. Bug reports and feature requests go through the [issue forms](https://github.com/rapira-rs/rapira/issues/new/choose); questions belong in [discussions](https://github.com/rapira-rs/rapira/discussions).

## Releases

Releases run through [release-please](https://github.com/googleapis/release-please). Every merge to `main` updates a release pull request with the next version and the changelog. When a maintainer merges that PR, one pipeline run tags the release, builds Linux x86_64/aarch64 and macOS aarch64 artifacts for PHP 8.4 and 8.5, builds the docker images, and publishes tarballs, `.deb`/`.rpm` packages and checksums to GitHub Releases. If a release run fails, re-run its failed jobs; the release stays a draft until the publish job completes. After a green CI run on `main`, a separate workflow re-points the `nightly-php*` docker tags and, except on the release commit, refreshes the rolling `nightly` prerelease with tarballs.

The version bump and the changelog come from the title of every commit that lands on `main`, so each title follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `feat:` for a minor bump, `fix:` for a patch, `!` or a `BREAKING CHANGE:` footer for a breaking change (a minor bump while the version is below 1.0.0). A squash merge collapses a PR into one commit, so its title is the one that counts. Put the issue reference at the end, `fix: register shutdown functions once (#84)`. A `[#84]:` prefix hides the commit from release-please, which needs the type at the start of the title.
