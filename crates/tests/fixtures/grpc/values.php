<?php

use Rapira\Grpc\Call\Context;
use Rapira\Grpc\Call\Protocol;
use Rapira\Grpc\Exception\GrpcException;
use Rapira\Grpc\Metadata;
use Rapira\Grpc\MethodKind;
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

$cases = [
    ['name' => 'metadata rejects an empty key', 'run' => fn () => new Metadata(['' => ['v']])],
    ['name' => 'metadata rejects an upper-case key', 'run' => fn () => new Metadata(['X-A' => ['v']])],
    ['name' => 'metadata rejects a non-ASCII key', 'run' => fn () => new Metadata(["x-\xc3\xa9" => ['v']])],
    ['name' => 'metadata rejects a non-ASCII text value', 'run' => fn () => new Metadata(['x-a' => ["caf\xc3\xa9"]])],
    ['name' => 'metadata rejects a control byte in a text value', 'run' => fn () => new Metadata(['x-a' => ["a\tb"]])],
    [
        'name' => 'metadata accepts an empty text value',
        'run' => fn () => (new Metadata(['x-a' => ['']]))->values('x-a') === [''] ? 'ok' : 'changed',
    ],
    [
        'name' => 'metadata keeps any bytes under a -bin key',
        'run' => fn () => (new Metadata(['x-b-bin' => ["\x00\t\xff"]]))->values('x-b-bin') === ["\x00\t\xff"] ? 'ok' : 'changed',
    ],
    ['name' => 'metadata rejects an entry that is not a list', 'run' => fn () => new Metadata(['x-a' => 'v'])],
    ['name' => 'metadata rejects a value that is not a string', 'run' => fn () => new Metadata(['x-a' => [1]])],
    ['name' => 'metadata rejects a map of values', 'run' => fn () => new Metadata(['x-a' => ['k' => 'v']])],
    ['name' => 'metadata rejects a sparse list', 'run' => fn () => new Metadata(['x-a' => [1 => 'v']])],
    ['name' => 'values() is case-insensitive and ordered', 'run' => fn () => json_encode($sample()->values('X-A'))],
    ['name' => 'values() of an absent key', 'run' => fn () => json_encode($sample()->values('x-c'))],
    ['name' => 'values() finds a numeric key', 'run' => fn () => json_encode((new Metadata(['123' => ['a']]))->values('123'))],
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
    ['name' => 'context rejects a remote that is not an address', 'run' => fn () => $context('nope')],
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
