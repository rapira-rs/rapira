<?php

/** @generate-class-entries */

namespace {
    /**
     * Classic and worker modes. Flush the response to the client early; the script may
     * keep working after it. In dispatcher mode the unit that receive() returned
     * finalizes instead, so the call throws.
     */
    function rapira_finish_request(): bool {}
}

namespace Rapira {
    enum LogLevel
    {
        case Error;
        case Warning;
        case Info;
        case Debug;
        case Trace;
    }

    /** The mode the host launched this process in: the `mode` key of the plugin's pool table. */
    enum Mode
    {
        case Classic;
        case Worker;
        case Dispatcher;
    }

    /**
     * A unit of work from a dispatcher. Host-created; the finalizing verbs live on the
     * concrete type. Dropping the last reference to an unfinalized unit reports the loss
     * to the host, which fails it.
     */
    interface Work
    {
        public function isFinalized(): bool;

        public function isCancelled(): bool;
    }

    /** Immutable counter snapshot. Observability only. */
    interface DispatcherInfo
    {
        public function pendingCount(): int;

        public function activeCount(): int;
    }

    /**
     * The plugin surface this worker's pool serves. Plugins narrow receive(),
     * tryReceive() and getInfo() to their own types.
     */
    interface Dispatcher
    {
        public function name(): string;

        /**
         * Never blocks. Null means nothing available right now.
         *
         * @throws Exception\ClosedException
         */
        public function tryReceive(): ?Work;

        /**
         * @param int $timeout Microseconds; -1 waits indefinitely, 0 does not wait at all.
         * @throws Exception\TimeoutException
         * @throws Exception\ClosedException
         */
        public function receive(int $timeout = -1): Work;

        public function getInfo(): DispatcherInfo;
    }

    /**
     * An IP endpoint. The other arm of the address union is UnixAddress.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class InetAddress
    {
        public string $ip;
        public int $port;

        public function __construct(string $ip, int $port) {}
    }

    /**
     * A unix domain socket endpoint. $path is null for an unnamed peer.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class UnixAddress
    {
        public ?string $path;

        public function __construct(?string $path) {}
    }

    /**
     * What the handshake settled. The cert fields describe the client's certificate and
     * are null unless one was presented.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class Tls
    {
        public string $version;
        public string $cipher;
        public ?string $negotiatedProtocol;
        public ?string $requestedServerName;
        public ?string $certSerial;
        public ?string $certOrganization;
        public ?string $certFingerprint;

        public function __construct(
            string $version,
            string $cipher,
            ?string $negotiatedProtocol,
            ?string $requestedServerName,
            ?string $certSerial,
            ?string $certOrganization,
            ?string $certFingerprint,
        ) {}
    }

    /** The mode of this process. The same case for the life of the process. */
    function get_mode(): Mode {}

    /**
     * The same instance for the life of the process.
     *
     * @throws Exception\NoDispatcherError Called outside dispatcher mode.
     */
    function get_dispatcher(): Dispatcher {}

    /**
     * Hand one unit to $handler, which reads the superglobals and responds through
     * echo/header(). False means the host hands out no more requests to this
     * app instance: exit the loop.
     * Call it only from the boot script's top level; a call from a shutdown
     * function or a destructor is undefined.
     *
     * @throws Exception\NotInWorkerModeError Called outside worker mode.
     */
    function handle_request(callable $handler): bool {}

    function get_version(): string {}

    /**
     * The calling thread writes the record to the host log under the `app` target. It waits while the log reader does not read. Never throws.
     * A \Throwable under any key of $context is serialized structurally: json_encode()
     * sees only public state, and an exception keeps all of its own in private ones.
     */
    function log(string $message, LogLevel $level = LogLevel::Info, array $context = []): void {}

    /**
     * The live objects of this process, counted per class and keyed by $object::class.
     * The count references no object. A garbage cycle counts until the collector frees it.
     *
     * @return array<class-string, int<1, max>>
     */
    function count_objects(): array {}

    /**
     * The live objects of this process, held weakly: each object maps to null.
     * The population is that of count_objects(), without the returned map.
     *
     * @param string[] $classes Class or interface names, matched with instanceof. An empty
     *        list matches every object. The names are not autoloaded: a class that is not loaded
     *        matches no object.
     * @return \WeakMap<object, null>
     * @throws \TypeError An entry of $classes is not a string.
     */
    function get_objects(array $classes = []): \WeakMap {}
}
