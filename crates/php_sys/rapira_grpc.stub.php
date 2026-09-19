<?php

/** @generate-class-entries */

namespace Rapira\Grpc {
    interface GrpcDispatcher extends \Rapira\Dispatcher
    {
        public function tryReceive(): ?UnaryCall;
        public function receive(int $timeout = -1): UnaryCall;
        public function getInfo(): GrpcDispatcherInfo;
        public function getServices(): array;
    }

    interface GrpcDispatcherInfo extends \Rapira\DispatcherInfo {}

    interface Call extends \Rapira\Work
    {
        public function getContext(): Call\Context;
    }

    interface Responder extends \Rapira\Work
    {
        public function getResponseMetadata(): Responder\ResponseMetadata;
        public function fail(Status $status): void;
    }

    interface UnaryRequest extends Call
    {
        public function getMessage(): string;
    }

    interface UnaryResponder extends Responder
    {
        public function respond(string $message): void;
    }

    interface UnaryCall extends UnaryRequest, UnaryResponder {}

    /**
     * @strict-properties
     * @not-serializable
     */
    final readonly class Metadata implements \Countable, \IteratorAggregate
    {
        public array $entries;

        public function __construct(array $entries = []) {}

        public function values(string $name): array {}

        public function count(): int {}

        public function getIterator(): \Iterator {}
    }

    enum StatusCode: int
    {
        case Cancelled = 1;
        case Unknown = 2;
        case InvalidArgument = 3;
        case DeadlineExceeded = 4;
        case NotFound = 5;
        case AlreadyExists = 6;
        case PermissionDenied = 7;
        case ResourceExhausted = 8;
        case FailedPrecondition = 9;
        case Aborted = 10;
        case OutOfRange = 11;
        case Unimplemented = 12;
        case Internal = 13;
        case Unavailable = 14;
        case DataLoss = 15;
        case Unauthenticated = 16;
    }

    enum MethodKind: string
    {
        case Unary = 'unary';
        case ServerStreaming = 'server-streaming';
        case ClientStreaming = 'client-streaming';
        case BidiStreaming = 'bidi-streaming';

        public function isStreamingRequest(): bool {}

        public function isStreamingResponse(): bool {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final readonly class ErrorDetail
    {
        public string $typeUrl;
        public string $value;

        public function __construct(string $typeUrl, string $value) {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final readonly class Status
    {
        public StatusCode $code;
        public string $message;
        /** @var list<ErrorDetail> */
        public array $details;

        public function __construct(StatusCode $code, string $message = '', array $details = []) {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final readonly class MethodInfo
    {
        public string $name;
        public string $inputType;
        public string $outputType;
        public MethodKind $kind;

        public function __construct(string $name, string $inputType, string $outputType, MethodKind $kind) {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final readonly class ServiceInfo
    {
        public string $name;
        /** @var list<MethodInfo> */
        public array $methods;

        public function __construct(string $name, array $methods) {}
    }
}

namespace Rapira\Grpc\Call {
    enum Protocol: string
    {
        case Grpc = 'grpc';
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final readonly class Context
    {
        public string $method;
        public \Rapira\Grpc\Metadata $metadata;
        public ?float $deadline;
        public \Rapira\InetAddress|\Rapira\UnixAddress $remote;
        public ?\Rapira\Tls $tls;
        public Protocol $protocol;
        public float $receivedAt;

        public function __construct(string $method, \Rapira\Grpc\Metadata $metadata, ?float $deadline, \Rapira\InetAddress|\Rapira\UnixAddress $remote, ?\Rapira\Tls $tls, Protocol $protocol, float $receivedAt) {}
    }
}

namespace Rapira\Grpc\Responder {
    interface ResponseMetadata
    {
        public function addHeader(string $name, string $value): void;
        public function addBinaryHeader(string $name, string $bytes): void;
        public function addTrailer(string $name, string $value): void;
        public function addBinaryTrailer(string $name, string $bytes): void;
        public function headers(): \Rapira\Grpc\Metadata;
        public function trailers(): \Rapira\Grpc\Metadata;
    }
}

namespace Rapira\Internal\Grpc {
    /**
     * @strict-properties
     * @not-serializable
     */
    final class Dispatcher implements \Rapira\Grpc\GrpcDispatcher
    {
        private function __construct() {}
        public function name(): string {}
        public function tryReceive(): ?\Rapira\Grpc\UnaryCall {}
        public function receive(int $timeout = -1): \Rapira\Grpc\UnaryCall {}
        public function getInfo(): \Rapira\Grpc\GrpcDispatcherInfo {}
        public function getServices(): array {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final class DispatcherInfo implements \Rapira\Grpc\GrpcDispatcherInfo
    {
        private function __construct() {}
        public function pendingCount(): int {}
        public function activeCount(): int {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final class UnaryCall implements \Rapira\Grpc\UnaryCall
    {
        private function __construct() {}
        public function isFinalized(): bool {}
        public function isCancelled(): bool {}
        public function __destruct() {}
        public function getContext(): \Rapira\Grpc\Call\Context {}
        public function getMessage(): string {}
        public function getResponseMetadata(): \Rapira\Grpc\Responder\ResponseMetadata {}
        public function respond(string $message): void {}
        public function fail(\Rapira\Grpc\Status $status): void {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final class ResponseMetadata implements \Rapira\Grpc\Responder\ResponseMetadata
    {
        private function __construct() {}
        public function addHeader(string $name, string $value): void {}
        public function addBinaryHeader(string $name, string $bytes): void {}
        public function addTrailer(string $name, string $value): void {}
        public function addBinaryTrailer(string $name, string $bytes): void {}
        public function headers(): \Rapira\Grpc\Metadata {}
        public function trailers(): \Rapira\Grpc\Metadata {}
    }
}

namespace Rapira\Grpc\Exception {
    class GrpcException extends \RuntimeException implements \Rapira\Exception\RapiraThrowable
    {
        public readonly \Rapira\Grpc\Status $status;

        public function __construct(\Rapira\Grpc\StatusCode $code, string $message = '', array $details = []) {}
    }
}
