<?php

/** @generate-class-entries */

namespace Rapira\Http {
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

    /**
     * A field part of a multipart/form-data body: no `filename` in content-disposition.
     * Buffered in memory.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class FormField
    {
        public string $name;
        public string $value;
        /** @var array<string, list<string>> */
        public array $headers;

        public function __construct(string $name, string $value, array $headers) {}
    }

    /**
     * A file part of a multipart/form-data body. The host spooled the bytes to $tmpPath;
     * the file lives until the exchange finalizes, so rename() it to keep it.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class UploadedFile
    {
        public string $name;
        public string $clientFilename;
        public ?string $clientMediaType;
        /** @var array<string, list<string>> */
        public array $headers;
        public string $tmpPath;
        public int $size;

        public function __construct(
            string $name,
            string $clientFilename,
            ?string $clientMediaType,
            array $headers,
            string $tmpPath,
            int $size,
        ) {}
    }

    /**
     * A multipart/form-data body the host parsed as the upload streamed in.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class Multipart
    {
        /** @var list<FormField> */
        public array $fields;
        /** @var list<UploadedFile> */
        public array $files;

        public function __construct(array $fields, array $files) {}
    }

    /**
     * Request data as the host received it. Not a PSR-7 request.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class Request
    {
        public string $method;
        public string $uri;
        public string $target;
        public ?string $authority;
        public string $protocol;
        /** @var array<string, list<string>> */
        public array $headers;
        public string|Multipart $body;
        public \Rapira\InetAddress|\Rapira\UnixAddress $remote;
        public \Rapira\InetAddress|\Rapira\UnixAddress $server;
        public ?Tls $tls;
        public float $receivedAt;

        public function __construct(
            string $method,
            string $uri,
            string $target,
            ?string $authority,
            string $protocol,
            array $headers,
            string|Multipart $body,
            \Rapira\InetAddress|\Rapira\UnixAddress $remote,
            \Rapira\InetAddress|\Rapira\UnixAddress $server,
            ?Tls $tls,
            float $receivedAt,
        ) {}
    }

    /** The narrowing point where HTTP-specific counters land. */
    interface HttpDispatcherInfo extends \Rapira\DispatcherInfo
    {
    }

    /** One request/response exchange: the request data plus the verbs that answer it. */
    interface Exchange extends \Rapira\Work
    {
        public function getRequest(): Request;

        /**
         * $headers is array<string, list<string>>: one entry per value.
         *
         * @throws Exception\HeadAlreadyWrittenError
         * @throws \Rapira\Exception\WorkDiscardedException
         * @throws \ValueError
         */
        public function writeHead(int $status, array $headers = []): void;

        /**
         * @throws Exception\ContentLengthExceededError
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         */
        public function writeBody(string $content, bool $eos = true): void;

        /**
         * @throws Exception\FileNotSendableException
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         * @throws \ValueError
         */
        public function sendFile(string $path, int $offset = 0, ?int $length = null, bool $eos = true): void;

        /**
         * $trailers has the shape of $headers above.
         *
         * @throws Exception\HeadNotWrittenError
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         * @throws \ValueError
         */
        public function writeTrailers(array $trailers): void;

        /**
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         */
        public function flush(): void;
    }

    /** The HTTP plugin's dispatcher, from \Rapira\get_dispatcher(). */
    interface HttpDispatcher extends \Rapira\Dispatcher
    {
        public function tryReceive(): ?Exchange;

        public function receive(int $timeout = -1): Exchange;

        public function getInfo(): HttpDispatcherInfo;
    }
}

namespace Rapira\Http\Exception {
    /** A body write went past the content-length the head declared. */
    class ContentLengthExceededError extends \Error implements \Rapira\Exception\RapiraThrowable
    {
    }

    /** The final head had already been written. */
    class HeadAlreadyWrittenError extends \Error implements \Rapira\Exception\RapiraThrowable
    {
    }

    /** A trailer section with no committed final head. */
    class HeadNotWrittenError extends \Error implements \Rapira\Exception\RapiraThrowable
    {
    }

    /** The host cannot send the file. Raised before sendFile() writes anything. */
    class FileNotSendableException extends \RuntimeException implements \Rapira\Exception\RapiraThrowable
    {
    }
}

namespace Rapira\Internal\Http {
    /**
     * The extension's implementation of \Rapira\Http\HttpDispatcher. Host-created.
     *
     * @strict-properties
     * @not-serializable
     */
    final class Dispatcher implements \Rapira\Http\HttpDispatcher
    {
        /** Host-created: obtain it from \Rapira\get_dispatcher(). */
        private function __construct() {}

        public function name(): string {}

        public function tryReceive(): ?\Rapira\Http\Exchange {}

        public function receive(int $timeout = -1): \Rapira\Http\Exchange {}

        public function getInfo(): \Rapira\Http\HttpDispatcherInfo {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final class DispatcherInfo implements \Rapira\Http\HttpDispatcherInfo
    {
        /** Host-created. */
        private function __construct() {}

        public function pendingCount(): int {}

        public function activeCount(): int {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final class Exchange implements \Rapira\Http\Exchange
    {
        /** Host-created. */
        private function __construct() {}

        public function isFinalized(): bool {}

        public function isCancelled(): bool {}

        public function getRequest(): \Rapira\Http\Request {}

        public function writeHead(int $status, array $headers = []): void {}

        public function writeBody(string $content, bool $eos = true): void {}

        public function sendFile(string $path, int $offset = 0, ?int $length = null, bool $eos = true): void {}

        public function writeTrailers(array $trailers): void {}

        public function flush(): void {}
    }
}
