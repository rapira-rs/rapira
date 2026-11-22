# Contributing to Rapira

This repository contains the `rapira` server, the PHP SAPI, the protocol plugins and their build and operating notes. The public manual is in [rapira-rs/rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## Contributor access

Rapira uses [Vouch](https://github.com/mitchellh/vouch) to manage permission to submit pull requests. Contributors with repository write, maintain, or admin permission can submit pull requests. Other contributors need a maintainer's approval.

1. Open a [contributor access request](https://github.com/rapira-rs/rapira/issues/new?template=contributor-access.yml).
2. Describe your proposed contribution and provide evidence that a maintainer can review.
3. Wait for a maintainer to approve your access.
4. Open your pull request after the approval is recorded.

Vouch closes pull requests from contributors without approval. An approval applies to this repository. It does not grant repository write access or replace code review. A maintainer must also approve external fork CI runs under the repository's GitHub Actions policy.

You can report bugs and request features without Vouch approval.

### Manage contributor access

Maintainers record approvals in [`.github/VOUCHED.td`](.github/VOUCHED.td):

- Approve a contributor: add `github:username`.
- Withdraw approval: remove the contributor's entry.

Use the request author's GitHub username without `@`. Keep one entry per username. Usernames are case-insensitive. Update the entry if the account is renamed.

Commit the change with a signature and sign-off, for example `git commit -s -S -m "chore: update contributor access"`. Include the request issue link in the commit body. The decision takes effect when the change reaches `main`. Confirm the result in the request issue. The contributor can then open a new PR. A maintainer can also reopen a PR that Vouch closed.

The workflow checks PRs when they are opened or reopened. A list change does not affect PRs that are already open. To block a user, [block the account in the organization](https://docs.github.com/en/communities/maintaining-your-safety-on-github/blocking-a-user-from-your-organization). Then close the user's open issues and pull requests.

Repository administrators can find the activation requirements in [the Vouch setup guide](.github/vouch/README.md).

## Prerequisites

- Rust stable - `rust-toolchain.toml` selects the exact channel for you
- A C compiler (the build compiles `crates/sapi/*.c` and the C method shells of each plugin against the PHP headers)
- libclang for bindgen (`libclang-dev` on Debian/Ubuntu, `clang-devel` on Fedora, `clang` on Arch)
- PHP 8.4, 8.5 or 8.6 with `--enable-zts --enable-embed=shared --disable-zend-signals`. See the [runtime requirements](README.md#install) for multi-worker OPcache.

CI builds PHP directly from `php-src` on Linux and macOS. The version jobs use the release branches until all three minors have a release with the OPcache fix. The `master` jobs enable PHP debug checks. [The CI action](.github/actions/php-embed/action.yml) lists the system packages. [The shared configure flags](.github/php-configure-flags.txt) select the release extensions.

For a local build, run `make php PHP_SRC=/path/to/php-src` in Rapira with a dedicated PHP source checkout. The default install prefix is `$HOME/.local/share/php-zts`. Distribution NTS embed packages also work for one pool with one worker.

## Build

```sh
export PHP_CONFIG="$HOME/.local/share/php-zts/bin/php-config"
export LD_LIBRARY_PATH="$("$PHP_CONFIG" --prefix)/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" # Linux
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--cfg tokio_unstable"
cargo build --release
```

The Tokio file reader uses io_uring on Linux when the kernel permits it. Keep `--cfg tokio_unstable` when you set custom Rust flags. The Makefile, CI, and release builds include it.

Use the headers and library from the same PHP build. For a custom prefix on macOS, use `DYLD_LIBRARY_PATH` for its `lib` directory. The `make` test targets find `libphp.so` or `libphp.dylib` through `PHP_CONFIG`. A Linux package that only supplies `libphpX.Y.so` also needs a `libphp.so` symlink in the linker search path.

## Tests

For a PHP 8.4 source build, load OPcache in a `.ini` file under `PHP_INI_SCAN_DIR`. Set `zend_extension` to the absolute path of `opcache.so` in the directory from `php-config --extension-dir`. The end-to-end tests set `PHPRC` themselves.

```sh
make test
```

- `make test_unit` runs the workspace unit tests.
- `make test_e2e` builds the server and runs the end-to-end suite serially. Tests start a server and drive it through HTTP, gRPC and signals. `make test` runs both suites in order. Use `PHP_CONFIG=/path/to/nts/php-config CARGO_TARGET_DIR=target/nts make test` to test the NTS fallback separately.
- `cargo fuzz run --fuzz-dir crates/tests/tests/fuzz -s none multipart_raw crates/tests/tests/fuzz/corpus/multipart_raw crates/tests/tests/fuzz/seeds/multipart_raw -- -dict=crates/tests/tests/fuzz/dict/multipart_raw.dict -create_missing_dirs=1` - runs one fuzz target with its seeds and its dictionary until it finds a crash or you stop it. It needs `cargo install --locked cargo-fuzz@0.13.2`. The targets, their seeds and their dictionaries are in `crates/tests/tests/fuzz/`. The `Fuzz` workflow runs each target with its seeds and its dictionary for 60 s on each pull request, and for 30 min twice a week.
- `make coverage` - writes the line coverage of the unit and e2e suites to `lcov.info`. It needs `cargo install --locked cargo-llvm-cov@0.9.1` and `rustup component add llvm-tools-preview`.
- `make stubs` - maintainers only: regenerates each `*_arginfo.h` header under `crates/` from the `*.stub.php` stub next to it with PHP's `gen_stub.php`. Never edit the generated headers by hand.

Test placement: unit tests live inside their crate and use no fixture: no file on disk, no socket, no child process, no environment variable and no PHP. Every test that needs a fixture spawns the `rapira` binary and lives under `crates/tests/tests/e2e/` behind the `e2e` feature. The shared harness (the wire clients and the log readers) is in `crates/tests/src/`, and the fixtures are in `crates/tests/fixtures/` and `crates/tests/tests/e2e/fixtures/`.

## Lint and format

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p tests --features e2e --tests -- -D warnings
```

C sources (`*.c`, `*.h` under `crates/`) follow `.clang-format`.

## Repository layout

| Crate                  | Directory                        | Role                                                                                                                                                  |
| ---------------------- | -------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rapira_core`          | `src/`                           | the `rapira` binary: CLI, configuration, plugin startup, shared IO runtime, signals and shutdown |
| `rapira_php_build`     | `crates/php_build`               | build helper: `php-config` discovery and the C compile of the method shells, for `rapira_sapi` and every plugin with a PHP surface                    |
| `rapira_net`           | `crates/net`                     | listener binding and the async accept loop |
| `rapira_sapi`          | `crates/sapi`                    | the embed SAPI, PHP engine, worker pools, intake, watchdog and dispatcher interfaces |
| `rapira_config`        | `crates/config`                  | `[server]`, `[log]`, pool settings, listener addresses and config paths |
| `rapira_scoreboard`    | `crates/scoreboard`              | in-process pool counters and worker state |
| `rapira_observability` | `crates/observability`           | Prometheus metrics and the `/livez` and `/readyz` probes on a separate thread |
| `rapira_http`          | `crates/plugins/http`            | the http plugin: HTTP/1.1, the `Rapira\Http` classes, the `[http]` table ([README](crates/plugins/http/README.md))                                    |
| `rapira_grpc`          | `crates/plugins/grpc`            | the grpc plugin: gRPC, gRPC-Web and Connect, the `Rapira\Grpc` classes, the `[grpc]` table ([README](crates/plugins/grpc/README.md))                  |
| `rapira_static_files`  | `crates/middleware/static_files` | the `static` http middleware, a tower layer; each built-in http middleware is one crate under `crates/middleware`                                     |
| `rapira_grpc_auth`     | `crates/interceptors/auth`       | the `auth` grpc interceptor; each built-in grpc interceptor is one crate under `crates/interceptors`                                                  |
| `tests`                | `crates/tests`                   | the e2e suite, its harness and its fixtures                                                                                                           |

## Plugins

- A dispatcher plugin owns a pool and turns each request into a work unit that PHP pulls with `receive()`: the http plugin makes a `Rapira\Http\Exchange`, the grpc plugin makes a `Rapira\Grpc\UnaryCall`.
- A plugin without a pool, such as a KV client, has no support, and the contract lists its PHP acquisition path as open.
- A middleware is a tower layer that the http plugin applies around its inner service in config order. It never touches PHP.
- An interceptor is a connect-rust `Interceptor` that the grpc plugin registers on its service in config order. It never touches PHP.

A plugin is one crate under `crates/plugins` that implements `rapira_sapi::plugin::Plugin`. It owns its config table, its PHP stub, its C method shells, the Rust behind those methods, its work unit and its transport. The two plugin READMEs describe the crate layout.

To add a plugin:

- Create its crate under `crates/plugins`.
- In the root `Cargo.toml`, add the crate to the workspace `members`, to `[workspace.dependencies]` and to the `[dependencies]` of `rapira_core`.
- In `src/settings.rs`, add its table to `FileConfig` and its settings to `Settings`.
- In `settings` in `src/settings.rs`, call its `resolve`, and add its table to the check that refuses a file with no plugin table.
- In `serve` in `src/server.rs`, build the plugin with its pool.
- In `src/main.rs`, add its register function to `PLUGIN_REGISTERS`.

## Pull requests

Sign off your commits (`git commit -s`) and fill in the PR template. Bug reports and feature requests go through the [issue forms](https://github.com/rapira-rs/rapira/issues/new/choose); questions belong in [discussions](https://github.com/orgs/rapira-rs/discussions).

## Releases

Releases run through [release-please](https://github.com/googleapis/release-please). Every merge to `main` updates a release pull request with the next version and the changelog. When a maintainer merges that PR, one pipeline run tags the release, builds Linux x86_64/aarch64 and macOS aarch64 artifacts and the docker images for PHP 8.4, 8.5 and 8.6, and publishes tarballs, `.deb`/`.rpm` packages and checksums to GitHub Releases. If a release run fails, re-run its failed jobs; the release stays a draft until the publish job completes. After a green CI run on `main`, a separate workflow re-points the `nightly-php*` docker tags and, except on the release commit, refreshes the rolling `nightly` prerelease with tarballs.

The root `Dockerfile` builds `rapira` on `ghcr.io/rapira-rs/php:<minor>-zts` and installs the image extensions. The `PHP rebuild` workflow checks the recorded base image digest every 15 minutes. Tarballs, packages and container images must support two PHP workers with OPcache enabled.

The version bump and the changelog come from the title of every commit that lands on `main`, so each title follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `feat:` for a minor bump, `fix:` for a patch, `!` or a `BREAKING CHANGE:` footer for a breaking change (a minor bump while the version is below 1.0.0). A squash merge collapses a PR into one commit, so its title is the one that counts. Put the issue reference at the end, `fix: register shutdown functions once (#84)`. A `[#84]:` prefix hides the commit from release-please, which needs the type at the start of the title.
