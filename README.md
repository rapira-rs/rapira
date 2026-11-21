# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rapira-rs/rapira?utm_source=oss&utm_medium=github&utm_campaign=rapira-rs%2Frapira&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)

Rapira is a PHP application server written in Rust. It embeds the PHP interpreter and serves HTTP directly. Run existing applications in classic mode, or keep them in memory across requests with worker and dispatcher modes.

[Documentation](https://rapira.rs/docs/intro/) | [Quickstart](https://rapira.rs/docs/intro/quickstart) | [Configuration](https://rapira.rs/docs/configuration) | [Framework integration](https://rapira.rs/docs/frameworks/)

## Install

Rapira supports Linux and macOS. Download a build for PHP 8.4, PHP 8.5 or PHP 8.6 from [GitHub Releases](https://github.com/rapira-rs/rapira/releases). Each build includes its PHP interpreter library. See the [installation guide](https://rapira.rs/docs/intro/installation) for packages, tar archives, and checksums.

PHP 8.4 tarballs and packages include `opcache.so` beside `libphp`. Set `zend_extension` to its absolute path in the PHP configuration selected by `PHPRC`. Linux packages use `zend_extension=/usr/lib/rapira/opcache.so`. PHP 8.5 and 8.6 include OPcache in `libphp`. The PHP 8.6 builds have no `igbinary` and `redis`.

The Linux builds for PHP 8.6 need OpenSSL 3.3 or newer, for example Debian 13, Ubuntu 26.04 or RHEL 9.7. Debian 12 and Ubuntu 24.04 have OpenSSL 3.0: use the PHP 8.4 or PHP 8.5 builds there, or update OpenSSL.

### Docker

The image includes Rapira, PHP and common PHP extensions. Add your application:

```dockerfile
FROM ghcr.io/rapira-rs/rapira:php8.5
COPY . /app
```

The image starts `rapira serve /app/rapira.toml`. The app directory holds a `rapira.toml`:

```toml
[http]
listen = ":8000"

[http.pool]
entrypoint = "/app/public/index.php"
mode = "classic"
```

The tags are `php8.4`, `php8.5` and `php8.6`. Each image includes `bcmath`, `intl`, `mbstring`, `pdo_pgsql`, `pgsql` and `sodium`. The PHP 8.4 and 8.5 images also include `igbinary` and `redis` with igbinary serialization support. No release of these two extensions compiles on PHP 8.6. Add more extensions with `docker-php-ext-install`, as in the official PHP image.

The PHP images without Rapira are `ghcr.io/rapira-rs/php:<minor>-nts` and the thread-safe `ghcr.io/rapira-rs/php:<minor>-zts`, for example `ghcr.io/rapira-rs/php:8.5-nts`. The `-zts` images have no `redis`. When a PHP minor gets a patch release, a workflow builds its PHP images again, and then the Rapira images of the newest release on them. See the [Docker guide](https://rapira.rs/docs/intro/installation#docker) for image tags and PHP extensions.

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

Each app instance resolves the entrypoint path when it starts, enters the entrypoint directory and keeps that working directory until the next app start. A `chdir()` in one request applies to every later request of that app instance. In classic mode a worker resolves the path when it starts and at each reload. A deploy that re-points a symlink in the entrypoint path needs a reload (SIGHUP or SIGUSR2) right after the switch. The reload moves every worker to the new directory and the new code.

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

See [examples](examples/) for routing and streaming.

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

Outside contributors must request [contributor access](CONTRIBUTING.md#contributor-access) and wait for maintainer approval before opening a pull request. See [CONTRIBUTING.md](CONTRIBUTING.md) for build and test instructions. The documentation source is in [rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
