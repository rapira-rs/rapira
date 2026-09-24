# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rapira-rs/rapira?utm_source=oss&utm_medium=github&utm_campaign=rapira-rs%2Frapira&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)

Rapira is a PHP application server written in Rust. It embeds the PHP interpreter and serves HTTP directly. Run existing applications in classic mode, or keep them in memory across requests with worker and dispatcher modes.

[Documentation](https://rapira.rs/docs/intro/) | [Quickstart](https://rapira.rs/docs/intro/quickstart) | [Configuration](https://rapira.rs/docs/configuration) | [Framework integration](https://rapira.rs/docs/frameworks/)

## Install

Rapira supports Linux and macOS. Download a build for PHP 8.4 or PHP 8.5 from [GitHub Releases](https://github.com/rapira-rs/rapira/releases). Each build includes its PHP interpreter library. See the [installation guide](https://rapira.rs/docs/intro/installation) for packages, tar archives, and checksums.

PHP 8.4 tarballs and packages include `opcache.so` beside `libphp`. Set `zend_extension` to its absolute path in the PHP configuration selected by `PHPRC`. Linux packages use `zend_extension=/usr/lib/rapira/opcache.so`. PHP 8.5 includes OPcache in `libphp`.

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

Each HTTP example listens on `127.0.0.1:8000`. After it starts, run `curl http://127.0.0.1:8000/` in another terminal. Relative paths in `rapira.toml` resolve against its directory; the classic example's `entrypoint` is `public/index.php`, one directory below the file.

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

A worker resolves the entrypoint directory when it starts and keeps its working directory across requests. A `chdir()` in one request applies to every later request of that worker, until the worker process exits. Worker and dispatcher modes work the same way. A deploy that re-points a symlink in the entrypoint path needs a reload (SIGHUP or SIGUSR2), so new workers start in the new directory.

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

### gRPC

The [gRPC plugin](crates/plugins/grpc/README.md) serves unary RPCs from PHP over gRPC, gRPC-Web and Connect. A gRPC pool runs in dispatcher mode. This example listens on `127.0.0.1:50051`, not on port 8000. Save this as `echo.proto`:

```proto
syntax = "proto3";

package echo.v1;

message EchoMessage {
  string text = 1;
}

service EchoService {
  rpc Echo(EchoMessage) returns (EchoMessage);
}
```

Build the descriptor set with [buf](https://buf.build/docs/reference/cli/buf/build/):

```sh
buf build --as-file-descriptor-set -o echo.binpb
```

Save this as `grpc.php`. The method takes and returns `EchoMessage`, so the script answers with the request bytes. An empty message means that `text` is not set. A real service decodes the message with the classes that `protoc --php_out` generates.

```php
<?php
use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;
use Rapira\Grpc\UnaryCall;

$dispatcher = \Rapira\get_dispatcher();

try {
    while (true) {
        $call = $dispatcher->receive();

        try {
            if (!$call instanceof UnaryCall) {
                $call->fail(new Status(StatusCode::Unimplemented));
            } elseif ($call->getMessage() === '') {
                $call->fail(new Status(StatusCode::InvalidArgument, 'text is required'));
            } else {
                $call->respond($call->getMessage());
            }
        } catch (WorkDiscardedException) {
        }
    }
} catch (ClosedException) {
}
```

Save this as `rapira.toml`:

```toml
[grpc]
listen = "127.0.0.1:50051"
descriptor_set = "echo.binpb"
services = ["echo.v1.EchoService"]

[grpc.pool]
entrypoint = "grpc.php"
```

```sh
rapira serve rapira.toml
```

Call the method with Connect JSON. The answer is `{"text":"hi"}`.

```sh
curl -H 'Content-Type: application/json' -d '{"text":"hi"}' http://127.0.0.1:50051/echo.v1.EchoService/Echo
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build and test instructions. The documentation source is in [rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
