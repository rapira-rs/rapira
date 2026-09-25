<?php

$d = \Rapira\get_dispatcher();

try {
    clone $d;
    $clone = 'allowed';
} catch (\Error) {
    $clone = 'blocked';
}

$method = static fn ($m): array => [
    'class' => $m::class,
    'name' => $m->name,
    'inputType' => $m->inputType,
    'outputType' => $m->outputType,
    'kind' => $m->kind->value,
];

// No call to answer here, so the results travel out through the app log.
\Rapira\log('dispatcher', context: [
    'class' => $d::class,
    'name' => $d->name(),
    'same' => $d === \Rapira\get_dispatcher(),
    'grpc' => $d instanceof \Rapira\Grpc\GrpcDispatcher,
    'base' => $d instanceof \Rapira\Dispatcher,
    'clone' => $clone,
    'info' => $d->getInfo()::class,
    'mode' => \Rapira\get_mode()->name,
    'services' => array_map(
        static fn ($s): array => [
            'class' => $s::class,
            'name' => $s->name,
            'methods' => array_map($method, $s->methods),
        ],
        $d->getServices(),
    ),
]);
