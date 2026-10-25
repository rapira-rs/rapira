# Rapira

Rapira is a PHP application server. A Rust host embeds the PHP interpreter in forked worker processes and hands them HTTP and gRPC work. This glossary names the concepts that the host, the PHP contract and the docs share.

## Processes

**Master**:
The single parent process. It binds the listeners, forks the workers of every pool and supervises them.
_Avoid_: parent process, daemon, main process

**Worker**:
One forked process with one PHP interpreter. It serves the units of one pool.
_Avoid_: child, PHP process; never the app code or a thread

**Pool**:
The worker set of one plugin, with its entrypoint, mode and process count.
_Avoid_: worker group, cluster, farm

**Recycle**:
The replacement of the app instance inside a worker by a fresh one, after max requests. The worker process stays.
_Avoid_: restart, respawn, reboot

**Drain**:
The end of a worker. It pulls no new units, finishes the units it holds within a grace period, then exits.
_Avoid_: graceful shutdown, graceful stop, wind down

**Scoreboard**:
The shared record of every worker's state and counters, one slot per worker.
_Avoid_: status table, stats, registry

## Sides of the boundary

**Host**:
The Rust side of the boundary: the master, and in a worker everything that is not the app.
_Avoid_: runtime, engine, native side, SAPI (for the side as a whole)

**App**:
The PHP application code that a worker runs from the entrypoint.
_Avoid_: worker (for code), script, userland, user code

**Entrypoint**:
The PHP file a pool runs.
_Avoid_: bootstrap, script, index file

**Mode**:
The way the workers of a pool run the app: classic, worker or dispatcher.
_Avoid_: runtime mode, FPM mode

**Classic mode**:
The app runs the entrypoint once per unit.

**Worker mode**:
The app runs the entrypoint once and serves units through a handler and the SAPI superglobals.

**Dispatcher mode**:
The app runs the entrypoint once and receives units as objects from a dispatcher.

**Plugin**:
A protocol the host serves with its own pool, listener and dispatcher: http or grpc.
_Avoid_: module, extension (that is a PHP extension), transport

**Middleware (HTTP) / Interceptor (gRPC)**:
One concept with a protocol-conventional name on each side: a host-side stage in front of a plugin's dispatch, applied in list order with the first entry outermost.
_Avoid_: filter, hook, guard, plugin (for a stage), the other protocol's word

## Units of work

**Unit**:
One piece of work the host hands to the app: an exchange for http, a call for grpc. `Work` is its PHP interface.
_Avoid_: job, task, message, request (for the generic concept)

**Exchange**:
The http unit: one request and the response that answers it.
_Avoid_: request (for the whole), transaction, context, connection

**Call**:
The grpc unit: one invocation of one method, unary or streaming.
_Avoid_: request, RPC (as a noun for the unit), exchange

**Dispatcher**:
The per-plugin object from which the app receives units in dispatcher mode. One per process.
_Avoid_: queue, receiver, handler, server

**Intake**:
The bounded queue of a worker between the plugin and the app. Units wait here until the app pulls them.
_Avoid_: worker queue, job queue, mailbox, backlog (that is the kernel listen queue)

**Pull**:
The app taking the next unit from the intake.
_Avoid_: poll, fetch, dequeue

**Pending**:
A unit in the intake that no pull has taken yet.

**Active**:
A unit that a pull took and that is not finalized.
_Avoid_: in flight (for the counter), running

**Cycle**:
One run of the entrypoint in a worker, from startup to shutdown. In classic mode a cycle serves one unit; in worker and dispatcher modes it serves many.
_Avoid_: request (the PHP SAPI sense), run, iteration, session, the garbage collector's cycle

**Boot**:
The phase before serving. For the server: the master binds listeners and loads schemas before the fork. For a cycle: the run of the entrypoint before the app's first pull. A fatal error in the boot is a boot failure, not an app failure.
_Avoid_: startup, bootstrap, warm-up, init

**Shed**:
The host's refusal of a unit the worker cannot serve: 503 for http, UNAVAILABLE for grpc.
_Avoid_: reject, drop, throttle, rate limit

**Cancelled**:
A unit whose result is no longer wanted: the client left, the deadline passed, or the host closed the unit.
_Avoid_: aborted, gone, timed out (for the state)

**Discarded**:
A unit closed without an answer, by the host (drain, deadline, client gone) or by the app.
_Avoid_: dropped, aborted

**Finalized**:
A unit with no further outcome possible: it was answered, discarded or cancelled.
_Avoid_: completed, done, closed, committed

**Lost**:
A unit the app released without finalizing it. The worker stays up, and the host fails the unit.
_Avoid_: abandoned, leaked, dropped
