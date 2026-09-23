# Examples

One file per run mode - two for the dispatcher. Use an installed `rapira`, or build one with `cargo build` and take `target/debug/rapira`.

The shipped file runs `dispatcher-sync.php`. To run another example, set `http.pool.entrypoint` and `http.pool.mode` in `examples/rapira.toml` or in a copy of it:

```sh
rapira serve examples/rapira.toml
```

| Entrypoint             | Mode         | What it shows                                                            |
| ---------------------- | ------------ | ------------------------------------------------------------------------ |
| `classic.php`          | `classic`    | one script execution per request                                         |
| `worker.php`           | `worker`     | resident script, a handler closure runs per request                      |
| `dispatcher-sync.php`  | `dispatcher` | resident script, one request at a time on a blocking `receive()`         |
| `dispatcher-async.php` | `dispatcher` | resident script, a fiber per request with `tryReceive()` between resumes |

All of them listen on 127.0.0.1:8000 by default. Classic and worker answer any path; the dispatcher examples route:

```sh
curl http://127.0.0.1:8000/                # hello
curl -d 'ping' http://127.0.0.1:8000/echo  # echoes the request body back
curl http://127.0.0.1:8000/boom            # 500 from a handler failure
curl http://127.0.0.1:8000/nope            # 404 for anything unrouted
curl http://127.0.0.1:8000/stream          # chunked streaming
```
