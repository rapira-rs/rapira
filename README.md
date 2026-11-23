# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rapira-rs/rapira?utm_source=oss&utm_medium=github&utm_campaign=rapira-rs%2Frapira&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)

Rapira is a PHP application server written in Rust. One server process runs the HTTP and gRPC pools. Each pool has a queue and a fixed number of PHP interpreter threads. Run existing applications in classic mode, or keep them in memory across requests with worker and dispatcher modes.

[Documentation](https://rapira.rs/docs/intro/) | [Quickstart](https://rapira.rs/docs/intro/quickstart) | [Configuration](https://rapira.rs/docs/configuration) | [Framework integration](https://rapira.rs/docs/frameworks/)

## Install

Rapira supports Linux and macOS. The tarballs and Linux packages bundle a thread-safe (ZTS) PHP interpreter. See [GitHub Releases](https://github.com/rapira-rs/rapira/releases) for downloads and checksums. Multiple workers with OPcache enabled require the [PHP thread-locking fix](https://github.com/php/php-src/commit/36c6d1f500f7ec477f6eae0b0f5c46b041d82347). Rapira checks for PHP 8.4.29, 8.5.14 or 8.6.1 or later in the corresponding minor.

PHP 8.4 tarballs and packages include `opcache.so` beside `libphp`. Set `zend_extension` to its absolute path in the PHP configuration selected by `PHPRC`. Linux packages use `zend_extension=/usr/lib/rapira/opcache.so`. PHP 8.5 and 8.6 include OPcache in `libphp`. The PHP 8.4 and 8.5 builds include `igbinary`. Rapira does not bundle the `redis` extension.

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

The tags are `php8.4`, `php8.5` and `php8.6`. Each image includes `bcmath`, `intl`, `mbstring`, `pdo_pgsql`, `pgsql` and `sodium`. The PHP 8.4 and 8.5 images also include `igbinary`. Use ZTS-compatible extensions when you add to the image with `docker-php-ext-install`.

Rapira uses the [official PHP images](https://hub.docker.com/_/php): `php:8.4-zts-trixie`, `php:8.5-zts-trixie` and `php:8.6-cli-trixie`. The PHP 8.6 CLI image includes ZTS and the embed library. See the [Docker guide](https://rapira.rs/docs/intro/installation#docker) for Rapira image tags and PHP extensions.

### Run the server

Run `rapira serve /path/to/rapira.toml` in the foreground. With systemd, use this command as `ExecStart` and set `Restart=on-failure`. In Docker, use `docker run --init` so that an init process reaps orphaned child processes. Set memory and CPU limits on the service or container.

`[http.pool].workers` and `[grpc.pool].workers` set the interpreter count for each pool. Both plugins share the IO threads set by `[server].io_threads`. A build linked to NTS PHP runs one worker, warns if the configured count is higher, and refuses a second PHP pool. With ZTS, PHP working directories are per thread. The process ID, `umask()` and `setlocale()` are shared.

Send `SIGUSR1` to the server for pool and worker status. Send `SIGHUP` or `SIGUSR2` to reload the apps. `SIGTERM`, `SIGINT` and `SIGQUIT` stop the listeners and drain open connections. `[server].stop_timeout_secs` sets the stop deadline (default: 30 seconds). A second `SIGTERM` or `SIGINT` forces exit 130.

`request_terminate_timeout_secs` sets a pool's wall-clock limit for a unit. At the limit, the watchdog interrupts PHP and wakes a blocked response write. Native IO can continue waiting. Set timeouts on sockets and database calls. After five more seconds, Rapira logs a still-blocked worker once. Other workers keep serving. The blocked worker can recycle its app when the call returns. Rapira exits **124** only if every worker of one pool is blocked after the grace period.

On platforms with `SIGRTMAX`, an enabled watchdog reserves that signal before workers start. Startup fails if the main thread blocks it or the process already handles or ignores it. Apps and extensions must leave the signal available. macOS uses PHP's interrupt flags without a native wake signal.

If `[server].pidfile` is set, a direct exit **70** (a worker reached its boot failure limit before its pool served), **101** (a PHP worker thread panicked), or **124** can leave the pidfile on disk. The next start overwrites it.

## Usage

Each HTTP example listens on `127.0.0.1:8000`. After it starts, run `curl http://127.0.0.1:8000/` in another terminal. Relative paths in `rapira.toml` resolve against its directory; the classic example's `entrypoint` is `public/index.php`, one directory below the file.

On Linux, a ZTS PHP build with per-thread timers counts wall-clock time toward `max_execution_time`. A `sleep()` in PHP uses this budget. Rapira pauses the timer while PHP waits for the next unit or for room in a response stream. Set `execution_clock = "cpu"` in a pool to count only the CPU time of the worker thread. Then a `sleep()` or a wait for a database does not use the budget. The setting has an effect only on Linux with per-thread timers. If the PHP build has no per-thread timers and the server runs several workers, Rapira sets `max_execution_time=0` and `max_input_time=-1`. The pool's wall-clock request limit still applies.

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

Each classic request starts in the entrypoint directory. In worker and dispatcher modes, a `chdir()` applies to later units of the same app instance. A relative `sendFile()` path starts in the PHP working directory.

At the end of a classic request or an app instance, PHP restores the directory it saved before running the entrypoint. This is usually the directory where Rapira started. PHP then runs [shutdown callbacks](https://www.php.net/manual/en/function.register-shutdown-function.php#refsect1-function.register-shutdown-function-notes) and final destructors. On NTS, a `chdir()` during shutdown can affect the next request's shutdown directory. A callback registered inside a worker request handler runs at the end of that unit, in the app's current directory. Use an absolute path in shutdown code, for example `__DIR__ . '/var/last.log'`.

On ZTS, each PHP thread has its own working directory. `proc_open()` without a `$cwd` starts the child in the directory where Rapira started. Pass `getcwd()` as `$cwd` to use the app's directory. `exec()`, `shell_exec()` and `popen()` use the PHP working directory.

Rapira resolves the entrypoint path at worker startup, at each app restart and on a classic reload. After you switch a symlink in that path, send SIGHUP or SIGUSR2 so that every worker loads the new release.

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
