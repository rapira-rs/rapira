<?php

/** @generate-class-entries */

namespace Rapira\Grpc {
    /** The gRPC status codes without OK. The values are the wire numbers. */
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

    /** The streaming shape of a method: one of the four gRPC method kinds. */
    enum MethodKind: string
    {
        case Unary = 'unary';
        case ServerStreaming = 'server-streaming';
        case ClientStreaming = 'client-streaming';
        case BidiStreaming = 'bidi-streaming';

        /** True when the calls of this method are a StreamingRequest. */
        public function isStreamingRequest(): bool {}

        /** True when the calls of this method are a StreamingResponder. */
        public function isStreamingResponse(): bool {}
    }

    /**
     * One packed message of Status::$details: the two fields of a google.protobuf.Any.
     * $value holds the raw message bytes. The host does not read them.
     *
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
     * The google.rpc.Status triple that Responder::fail() takes. The host encodes it for the
     * protocol of the call. The client gets $message unchanged.
     *
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
     * Multivalued metadata. Keys are lower-case ASCII. Values of `-bin` keys are raw bytes.
     * Other values are printable ASCII.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class Metadata implements \Countable, \IteratorAggregate
    {
        /** @var array<string, list<string>> */
        public array $entries;

        /**
         * @throws \ValueError A key is empty, not ASCII or not lower-case, or a text value is not printable ASCII.
         * @throws \TypeError An entry is not a list of strings.
         */
        public function __construct(array $entries = []) {}

        /**
         * All values of the key, in arrival order. The lookup ignores case. An absent key gives [].
         *
         * @return list<string>
         */
        public function values(string $name): array {}

        /** The number of keys. */
        public function count(): int {}

        public function getIterator(): \Iterator {}
    }

    /**
     * One method of a ServiceInfo. $name is the bare method name. $inputType and $outputType are
     * fully qualified message names.
     *
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
     * One service that the dispatcher serves. $name is fully qualified.
     *
     * @strict-properties
     * @not-serializable
     */
    final readonly class ServiceInfo
    {
        public string $name;
        /** @var list<MethodInfo> In descriptor order. */
        public array $methods;

        public function __construct(string $name, array $methods) {}
    }

    /** The counters of the gRPC dispatcher. */
    interface GrpcDispatcherInfo extends \Rapira\DispatcherInfo
    {
    }

    /** The reading side of one RPC. The call from GrpcDispatcher::receive() is also a Responder. */
    interface Call extends \Rapira\Work
    {
        public function getContext(): Call\Context;
    }

    /** The answering side of one RPC. All method kinds fail the same way. */
    interface Responder extends \Rapira\Work
    {
        /** The response headers and trailers of this call. */
        public function getResponseMetadata(): Responder\ResponseMetadata;

        /**
         * Finalize the call with a status error. The response metadata is taken at this point.
         *
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         */
        public function fail(Status $status): void;
    }

    /** One request message, available before dispatch. */
    interface UnaryRequest extends Call
    {
        /** The binary protobuf encoding of the input message. */
        public function getMessage(): string;
    }

    /** A stream of request messages. The stream is still open when the worker takes the call. */
    interface StreamingRequest extends Call
    {
        /** The same stream on each call. All readers move one shared cursor. */
        public function getMessages(): Call\MessageStream;
    }

    /** One response message finalizes the call. */
    interface UnaryResponder extends Responder
    {
        /**
         * Finalize the call with the binary protobuf encoding of the output message.
         * The response metadata is taken at this point.
         *
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         */
        public function respond(string $message): void;
    }

    /** A generator of response messages finalizes the call. */
    interface StreamingResponder extends Responder
    {
        /**
         * Send each yielded string as one message, and return when the generator ends. The headers
         * are taken at the first yield, the trailers at the end. A throwable from the generator
         * leaves this method and the call stays unfinalized.
         *
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \Rapira\Exception\WorkDiscardedException
         */
        public function respond(\Generator $messages): void;
    }

    /** One request message and one response message. */
    interface UnaryCall extends UnaryRequest, UnaryResponder
    {
    }

    /** One request message and a stream of response messages. */
    interface ServerStreamingCall extends UnaryRequest, StreamingResponder
    {
    }

    /** A stream of request messages and one response message. */
    interface ClientStreamingCall extends StreamingRequest, UnaryResponder
    {
    }

    /** A stream of request messages and a stream of response messages. */
    interface BidiStreamingCall extends StreamingRequest, StreamingResponder
    {
    }

    /** The dispatcher of the gRPC plugin, from \Rapira\get_dispatcher(). */
    interface GrpcDispatcher extends \Rapira\Dispatcher
    {
        public function tryReceive(): UnaryCall|ServerStreamingCall|ClientStreamingCall|BidiStreamingCall|null;

        public function receive(int $timeout = -1): UnaryCall|ServerStreamingCall|ClientStreamingCall|BidiStreamingCall;

        public function getInfo(): GrpcDispatcherInfo;

        /**
         * The services of the `grpc` section of rapira.toml.
         *
         * @return list<ServiceInfo>
         */
        public function getServices(): array;
    }
}

namespace Rapira\Grpc\Call {
    /** The protocol that the client used. For logs only: the host removes the differences. */
    enum Protocol: string
    {
        case Grpc = 'grpc';
        case GrpcWeb = 'grpc-web';
        case Connect = 'connect';
        case Rest = 'rest';
    }

    /**
     * The request side of one call. $method is the full name, `package.Service/Method`.
     * $metadata holds application keys only. $deadline and $receivedAt are Unix timestamps with
     * microsecond precision. $deadline is null when no deadline applies. $tls is null when the
     * listener did not terminate TLS.
     *
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

        public function __construct(
            string $method,
            \Rapira\Grpc\Metadata $metadata,
            ?float $deadline,
            \Rapira\InetAddress|\Rapira\UnixAddress $remote,
            ?\Rapira\Tls $tls,
            Protocol $protocol,
            float $receivedAt,
        ) {}
    }

    /** The request messages of one StreamingRequest call, in arrival order. */
    interface MessageStream extends \IteratorAggregate
    {
        /**
         * @param int $timeout Microseconds; -1 waits indefinitely, 0 does not wait at all.
         * @throws \Rapira\Exception\TimeoutException
         * @throws \Rapira\Exception\ClosedException The client half-closed the stream.
         * @throws \Rapira\Exception\WorkDiscardedException The host closed the call.
         */
        public function next(int $timeout = -1): string;

        /**
         * Never waits. Null means no message at this moment.
         *
         * @throws \Rapira\Exception\ClosedException The client half-closed the stream.
         * @throws \Rapira\Exception\WorkDiscardedException The host closed the call.
         */
        public function tryNext(): ?string;

        /** Iterates with next(-1) until the half-close, on the same cursor. */
        public function getIterator(): \Traversable;
    }
}

namespace Rapira\Grpc\Responder {
    /**
     * The response headers and trailers of one call. The host takes them when the call commits
     * them. A repeated name adds a value. The name is lower-cased.
     */
    interface ResponseMetadata
    {
        /**
         * @throws \Rapira\Grpc\Exception\HeadersAlreadyCommittedError
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \ValueError The name is reserved or has the `-bin` suffix, or the value is not printable ASCII.
         */
        public function addHeader(string $name, string $value): void;

        /**
         * @throws \Rapira\Grpc\Exception\HeadersAlreadyCommittedError
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \ValueError The name is reserved or does not have the `-bin` suffix.
         */
        public function addBinaryHeader(string $name, string $bytes): void;

        /**
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \ValueError As addHeader().
         */
        public function addTrailer(string $name, string $value): void;

        /**
         * @throws \Rapira\Exception\AlreadyFinalizedError
         * @throws \ValueError As addBinaryHeader().
         */
        public function addBinaryTrailer(string $name, string $bytes): void;

        public function headers(): \Rapira\Grpc\Metadata;

        public function trailers(): \Rapira\Grpc\Metadata;
    }
}

namespace Rapira\Grpc\Exception {
    /** A Status as a throwable. The host does not catch it: an adapter calls $call->fail($e->status). */
    class GrpcException extends \RuntimeException implements \Rapira\Exception\RapiraThrowable
    {
        public readonly \Rapira\Grpc\Status $status;

        /** @param list<\Rapira\Grpc\ErrorDetail> $details */
        public function __construct(\Rapira\Grpc\StatusCode $code, string $message = '', array $details = []) {}
    }

    /** A header was added after the first yield of a streaming response sent the headers. */
    class HeadersAlreadyCommittedError extends \Error implements \Rapira\Exception\RapiraThrowable
    {
    }
}

namespace Rapira\Internal\Grpc {
    /**
     * The extension's implementation of \Rapira\Grpc\GrpcDispatcher. Host-created.
     *
     * @strict-properties
     * @not-serializable
     */
    final class Dispatcher implements \Rapira\Grpc\GrpcDispatcher
    {
        /**
         * Host-created: obtain it from \Rapira\get_dispatcher().
         *
         * @implementation-alias Rapira\Internal\Http\Dispatcher::__construct
         */
        private function __construct() {}

        public function name(): string {}

        /** @implementation-alias Rapira\Internal\Http\Dispatcher::tryReceive */
        public function tryReceive(): \Rapira\Grpc\UnaryCall|\Rapira\Grpc\ServerStreamingCall|\Rapira\Grpc\ClientStreamingCall|\Rapira\Grpc\BidiStreamingCall|null {}

        /** @implementation-alias Rapira\Internal\Http\Dispatcher::receive */
        public function receive(int $timeout = -1): \Rapira\Grpc\UnaryCall|\Rapira\Grpc\ServerStreamingCall|\Rapira\Grpc\ClientStreamingCall|\Rapira\Grpc\BidiStreamingCall {}

        /** @implementation-alias Rapira\Internal\Http\Dispatcher::getInfo */
        public function getInfo(): \Rapira\Grpc\GrpcDispatcherInfo {}

        public function getServices(): array {}
    }

    /**
     * @strict-properties
     * @not-serializable
     */
    final class DispatcherInfo implements \Rapira\Grpc\GrpcDispatcherInfo
    {
        /**
         * Host-created.
         *
         * @implementation-alias Rapira\Internal\Http\DispatcherInfo::__construct
         */
        private function __construct() {}

        /** @implementation-alias Rapira\Internal\Http\DispatcherInfo::pendingCount */
        public function pendingCount(): int {}

        /** @implementation-alias Rapira\Internal\Http\DispatcherInfo::activeCount */
        public function activeCount(): int {}
    }

    /**
     * The extension's implementation of \Rapira\Grpc\UnaryCall. Host-created.
     *
     * @strict-properties
     * @not-serializable
     */
    final class UnaryCall implements \Rapira\Grpc\UnaryCall
    {
        /**
         * Host-created.
         *
         * @implementation-alias Rapira\Internal\Http\Exchange::__construct
         */
        private function __construct() {}

        public function isFinalized(): bool {}

        public function isCancelled(): bool {}

        public function getContext(): \Rapira\Grpc\Call\Context {}

        public function getMessage(): string {}

        public function getResponseMetadata(): \Rapira\Grpc\Responder\ResponseMetadata {}

        public function respond(string $message): void {}

        public function fail(\Rapira\Grpc\Status $status): void {}
    }

    /**
     * The response headers and trailers of one UnaryCall. It lives as long as its call.
     *
     * @strict-properties
     * @not-serializable
     */
    final class ResponseMetadata implements \Rapira\Grpc\Responder\ResponseMetadata
    {
        /**
         * Host-created.
         *
         * @implementation-alias Rapira\Internal\Http\Exchange::__construct
         */
        private function __construct() {}

        public function addHeader(string $name, string $value): void {}

        public function addBinaryHeader(string $name, string $bytes): void {}

        public function addTrailer(string $name, string $value): void {}

        public function addBinaryTrailer(string $name, string $bytes): void {}

        public function headers(): \Rapira\Grpc\Metadata {}

        public function trailers(): \Rapira\Grpc\Metadata {}
    }
}
