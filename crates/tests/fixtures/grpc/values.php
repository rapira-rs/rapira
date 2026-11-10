<?php

use Rapira\Exception\RapiraThrowable;
use Rapira\Grpc\Call\Context;
use Rapira\Grpc\Call\Protocol;
use Rapira\Grpc\ErrorDetail;
use Rapira\Grpc\Exception\GrpcException;
use Rapira\Grpc\Exception\HeadersAlreadyCommittedError;
use Rapira\Grpc\Metadata;
use Rapira\Grpc\MethodInfo;
use Rapira\Grpc\MethodKind;
use Rapira\Grpc\ServiceInfo;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;
use Rapira\InetAddress;

// Every class is touched inside a case, so a missing class fails its case and not the script.
$sample = static fn (): Metadata => new Metadata(['x-a' => ['1', '2'], 'x-b-bin' => ["\x00\xff"]]);
$context = static fn (mixed $remote): Context => new Context(
    'rapira.test.v1.EchoService/Echo',
    new Metadata(),
    null,
    $remote,
    null,
    Protocol::Grpc,
    1.0,
);

// The class and the message of the contract's constructor body.
$rejects = static function (array $entries): string {
    try {
        new Metadata($entries);
        return 'accepted';
    } catch (\TypeError|\ValueError $e) {
        return $e::class . ': ' . $e->getMessage();
    }
};

$cases = [
    ['name' => 'metadata rejects an empty key', 'run' => fn () => $rejects(['' => ['v']])],
    ['name' => 'metadata rejects an upper-case key', 'run' => fn () => $rejects(['X-A' => ['v']])],
    ['name' => 'metadata rejects a non-ASCII key', 'run' => fn () => $rejects(["x-\xc3\xa9" => ['v']])],
    ['name' => 'metadata rejects a non-ASCII text value', 'run' => fn () => $rejects(['x-a' => ["caf\xc3\xa9"]])],
    ['name' => 'metadata rejects a control byte in a text value', 'run' => fn () => $rejects(['x-a' => ["a\tb"]])],
    [
        'name' => 'metadata accepts an empty text value',
        'run' => fn () => (new Metadata(['x-a' => ['']]))->values('x-a') === [''] ? 'ok' : 'changed',
    ],
    [
        'name' => 'metadata keeps any bytes under a -bin key',
        'run' => fn () => (new Metadata(['x-b-bin' => ["\x00\t\xff"]]))->values('x-b-bin') === ["\x00\t\xff"] ? 'ok' : 'changed',
    ],
    ['name' => 'metadata rejects an entry that is not a list', 'run' => fn () => $rejects(['x-a' => 'v'])],
    ['name' => 'metadata rejects a value that is not a string', 'run' => fn () => $rejects(['x-a' => [1]])],
    ['name' => 'metadata rejects a map of values', 'run' => fn () => $rejects(['x-a' => ['k' => 'v']])],
    ['name' => 'metadata checks the list before the values', 'run' => fn () => $rejects(['x-a' => ["\x01", 'k' => 'v']])],
    ['name' => 'metadata names an int key', 'run' => fn () => $rejects([123 => 'v'])],
    ['name' => 'metadata rejects a sparse list', 'run' => fn () => $rejects(['x-a' => [1 => 'v']])],
    ['name' => 'values() is case-insensitive and ordered', 'run' => fn () => json_encode($sample()->values('X-A'))],
    ['name' => 'values() of an absent key', 'run' => fn () => json_encode($sample()->values('x-c'))],
    ['name' => 'values() finds a numeric key', 'run' => fn () => json_encode((new Metadata(['123' => ['a']]))->values('123'))],
    ['name' => 'values() finds a negative numeric key', 'run' => fn () => json_encode((new Metadata(['-123' => ['a']]))->values('-123'))],
    ['name' => 'values() keeps a leading-zero key', 'run' => fn () => json_encode((new Metadata(['01' => ['a'], 1 => ['b']]))->values('01'))],
    ['name' => 'values() accepts an empty lookup', 'run' => fn () => json_encode($sample()->values(''))],
    ['name' => 'values() finds a dash key', 'run' => fn () => json_encode((new Metadata(['-' => ['a']]))->values('-'))],
    ['name' => 'values() keeps an embedded NUL in a key', 'run' => fn () => json_encode((new Metadata(["x\0a" => ['a'], 'x' => ['b']]))->values("X\0A"))],
    [
        'name' => 'values() returns a list by value',
        'run' => static function (): string {
            $values = ['a'];
            $metadata = new Metadata(['x-a' => &$values]);
            $result = $metadata->values('x-a');
            $result[] = 'b';
            return json_encode([$result, $values]);
        },
    ],
    ['name' => 'count() counts keys', 'run' => fn () => (string) count($sample())],
    ['name' => 'getIterator() walks the entries', 'run' => fn () => implode(',', array_keys(iterator_to_array($sample())))],
    [
        'name' => 'method kind axes',
        'run' => fn () => implode(' ', array_map(
            static fn (MethodKind $k): string => $k->value . ':' . (int) $k->isStreamingRequest() . (int) $k->isStreamingResponse(),
            MethodKind::cases(),
        )),
    ],
    [
        'name' => 'grpc exception carries its status',
        'run' => static function (): string {
            $e = new GrpcException(StatusCode::NotFound, 'nf');
            return implode('|', [$e->status->code->name, $e->status->message, $e->getMessage(), $e->getCode()]);
        },
    ],
    [
        'name' => 'grpc exception subclass calls the parent',
        'run' => fn () => (new class ('ab') extends GrpcException {
            public function __construct(string $message)
            {
                parent::__construct(StatusCode::Aborted, $message);
            }
        })->status->code->name,
    ],
    [
        'name' => 'error detail keeps its pair',
        'run' => static function (): string {
            $d = new ErrorDetail('type.googleapis.com/google.rpc.RetryInfo', 'v');
            return implode('|', [$d->typeUrl, $d->value]);
        },
    ],
    [
        'name' => 'status keeps its triple',
        'run' => static function (): string {
            $s = new Status(StatusCode::NotFound, 'nf', [new ErrorDetail('t', 'v')]);
            return implode('|', [$s->code->name, $s->message, $s->details[0]->typeUrl]);
        },
    ],
    [
        'name' => 'method info keeps its four values',
        'run' => static function (): string {
            $m = new MethodInfo('m', 'in', 'out', MethodKind::ServerStreaming);
            return implode('|', [$m->name, $m->inputType, $m->outputType, $m->kind->value]);
        },
    ],
    [
        'name' => 'service info keeps its methods',
        'run' => static function (): string {
            $m = new MethodInfo('m', 'in', 'out', MethodKind::Unary);
            $s = new ServiceInfo('s', [$m, $m]);
            return $s->name . '|' . count($s->methods);
        },
    ],
    // The host reads the properties that the constructor sets.
    [
        'name' => 'status needs its constructor',
        'run' => fn () => (new \ReflectionClass(Status::class))->newInstanceWithoutConstructor()::class,
    ],
    [
        'name' => 'metadata needs its constructor',
        'run' => fn () => (string) count((new \ReflectionClass(Metadata::class))->newInstanceWithoutConstructor()),
    ],
    // Only \Rapira\get_dispatcher() creates the dispatcher.
    [
        'name' => 'dispatcher needs the host',
        'run' => fn () => (new \ReflectionClass(\Rapira\Internal\Grpc\Dispatcher::class))->newInstanceWithoutConstructor()::class,
    ],
    // Only a streaming response commits the headers before the finalization, and the host has the unary call only.
    [
        'name' => 'HeadersAlreadyCommittedError is an Error and a RapiraThrowable',
        'run' => static function (): string {
            $e = new HeadersAlreadyCommittedError();
            return get_parent_class($e) . '|' . var_export($e instanceof RapiraThrowable, true);
        },
    ],
    ['name' => 'context rejects a remote that is not an address', 'run' => fn () => $context('nope')],
    [
        'name' => 'context keeps its seven values',
        'run' => static function () use ($sample): string {
            $c = new Context(
                'rapira.test.v1.EchoService/Echo',
                $sample(),
                2.5,
                new InetAddress('203.0.113.7', 44123),
                null,
                Protocol::GrpcWeb,
                1722700000.25,
            );
            return implode('|', [
                $c->method,
                count($c->metadata),
                $c->deadline,
                $c->remote->ip,
                var_export($c->tls, true),
                $c->protocol->value,
                $c->receivedAt,
            ]);
        },
    ],
    [
        'name' => 'context keeps a null tls',
        'run' => fn () => var_export($context(new InetAddress('203.0.113.7', 44123))->tls, true),
    ],
];

// No response to write into here, so the results travel out through the app log.
foreach ($cases as $case) {
    try {
        $result = $case['run']();
    } catch (\Throwable $e) {
        $result = $e::class;
    }
    \Rapira\log('case', context: ['name' => $case['name'], 'result' => $result]);
}
