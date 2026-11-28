# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rapira-rs/rapira?utm_source=oss&utm_medium=github&utm_campaign=rapira-rs%2Frapira&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)

Rapira is a PHP application server written in Rust for Linux and macOS. One server process runs HTTP and gRPC pools of thread-safe (ZTS) PHP workers. Run existing PHP apps per request in classic mode, or keep them in memory across requests in worker and dispatcher modes.

[Documentation](https://rapira.rs/docs/intro/) | [Installation](https://rapira.rs/docs/intro/installation) | [Quickstart](https://rapira.rs/docs/intro/quickstart) | [Configuration](https://rapira.rs/docs/configuration) | [Frameworks](https://rapira.rs/docs/frameworks/)

## Install

Download binaries and packages from [GitHub Releases](https://github.com/rapira-rs/rapira/releases), or pull the container image `ghcr.io/rapira-rs/rapira:php8.5`. See the [installation guide](https://rapira.rs/docs/intro/installation) for packages, tarballs, [Docker](https://rapira.rs/docs/intro/installation#docker), and PHP requirements. See [build from source](https://rapira.rs/docs/intro/build-from-source) to compile Rapira yourself.

## Quickstart

Create `rapira.toml` in a directory with `public/index.php`:

```toml
[http]
listen = "127.0.0.1:8000"

[http.pool]
entrypoint = "public/index.php"
mode = "classic"
```

Start the server:

```sh
rapira serve rapira.toml
```

See the [quickstart guide](https://rapira.rs/docs/intro/quickstart) for complete examples and [examples](examples/) for sample configurations.

## Documentation

- **Execution modes**: [Overview](https://rapira.rs/docs/execution-modes), [Classic](https://rapira.rs/docs/classic), [Worker](https://rapira.rs/docs/worker), and [Dispatcher](https://rapira.rs/docs/dispatcher).
- **Protocols and middleware**: [HTTP](https://rapira.rs/docs/http), [gRPC](https://rapira.rs/docs/grpc) (gRPC, gRPC-Web, and Connect), and [Static files](https://rapira.rs/docs/static-files).
- **Operations**: [CLI](https://rapira.rs/docs/cli), [Configuration](https://rapira.rs/docs/configuration), [Process model](https://rapira.rs/docs/process-model), [Logging](https://rapira.rs/docs/logging), [Metrics and health checks](https://rapira.rs/docs/observability), and [Deployment](https://rapira.rs/docs/deployment).
- **Framework integration**: [Overview](https://rapira.rs/docs/frameworks/), [Symfony](https://rapira.rs/docs/frameworks/symfony), [Laravel](https://rapira.rs/docs/frameworks/laravel), and [Yii3](https://rapira.rs/docs/frameworks/yii3).

## Contributing

Outside contributors must request [contributor access](CONTRIBUTING.md#contributor-access) and wait for maintainer approval before opening a pull request. See [CONTRIBUTING.md](CONTRIBUTING.md) for build and test instructions. The documentation source is in [rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
