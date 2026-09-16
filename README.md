# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rapira-rs/rapira?utm_source=oss&utm_medium=github&utm_campaign=rapira-rs%2Frapira&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)

Rapira is a PHP application server written in Rust. It embeds the PHP interpreter and serves HTTP directly. Run existing applications in classic mode, or keep them in memory across requests with worker and dispatcher modes.

[Documentation](https://rapira.rs/docs/intro/) | [Quickstart](https://rapira.rs/docs/intro/quickstart) | [Configuration](https://rapira.rs/docs/configuration) | [Framework integration](https://rapira.rs/docs/frameworks/)

## Install

Rapira supports Linux and macOS. Download a build for PHP 8.4 or PHP 8.5 from [GitHub Releases](https://github.com/rapira-rs/rapira/releases). Each build includes its PHP interpreter library. See the [installation guide](https://rapira.rs/docs/intro/installation) for packages, tar archives, and checksums.

### Docker

Copy Rapira and its PHP library into your application image:

```dockerfile
FROM php:8.5-cli-trixie
COPY --from=ghcr.io/rapira-rs/rapira:php8.5 / /
RUN apt-get update \
    && xargs -r apt-get install -y --no-install-recommends < /usr/local/share/rapira/debian-packages.txt \
    && rm -rf /var/lib/apt/lists/*
COPY . /app
CMD ["rapira", "serve", "/app/rapira.toml"]
```

The app directory holds a `rapira.toml`:

```toml
[http]
listen = ":8000"

[http.pool]
entrypoint = "/app/public/index.php"
mode = "classic"
```

The payload includes `bcmath`, `intl`, `pdo_pgsql`, `pgsql`, `igbinary`, and `redis` with igbinary serialization support. The package manifest lists their runtime libraries. See the [Docker guide](https://rapira.rs/docs/intro/installation#docker) for image tags and PHP extensions.

## Usage

Each example listens on `127.0.0.1:8000`. After it starts, run `curl http://127.0.0.1:8000/` in another terminal. Relative paths in `rapira.toml` resolve against its directory; the classic example's `entrypoint` is `public/index.php`, one directory below the file.

### Classic

[Classic mode](https://rapira.rs/docs/classic) runs the script for each request. Save this as `public/index.php`:

```php
<?php
header('Content-Type: text/plain');
echo "Hello, {$_SERVER['REQUEST_URI']}\n";
```

Save this as `rapira.toml`:

```toml
[http]
listen = "127.0.0.1:8000"

[http.pool]
entrypoint = "public/index.php"
mode = "classic"
```

```sh
rapira serve rapira.toml
```

### Worker

[Worker mode](https://rapira.rs/docs/worker) calls a handler for each request and keeps application state in memory. Save this as `worker.php`:

```php
<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo "Hello, {$_SERVER['REQUEST_URI']}\n";
};

while (\Rapira\handle_request($handler)) {
}
```

Save this as `rapira.toml`:

```toml
[http]
listen = "127.0.0.1:8000"

[http.pool]
entrypoint = "worker.php"
mode = "worker"
```

```sh
rapira serve rapira.toml
```

### Dispatcher (default)

[Dispatcher mode](https://rapira.rs/docs/execution-modes#dispatcher) gives the application control of the request loop. Save this as `dispatcher.php`:

```php
<?php
use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;

$dispatcher = \Rapira\get_dispatcher();

try {
    while (true) {
        $exchange = $dispatcher->receive();

        try {
            $request = $exchange->getRequest();
            $exchange->writeHead(200, ['content-type' => ['text/plain']]);
            $exchange->writeBody("Hello, {$request->target}\n");
        } catch (WorkDiscardedException) {
        }
    }
} catch (ClosedException) {
}
```

Save this as `rapira.toml`:

```toml
[http]
listen = "127.0.0.1:8000"

[http.pool]
entrypoint = "dispatcher.php"
mode = "dispatcher"
```

```sh
rapira serve rapira.toml
```

See [examples](examples/) for routing, streaming, and asynchronous dispatch.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build and test instructions. The documentation source is in [rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
