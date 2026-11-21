<?php

$d = \Rapira\get_dispatcher();

try {
    clone $d;
    $clone = 'allowed';
} catch (\Error) {
    $clone = 'blocked';
}

// The boot has no unit to answer, so the results go to the app log.
\Rapira\log('dispatcher', context: [
    'class' => $d::class,
    'name' => $d->name(),
    'same' => $d === \Rapira\get_dispatcher(),
    'http' => $d instanceof \Rapira\Http\HttpDispatcher,
    'base' => $d instanceof \Rapira\Dispatcher,
    'clone' => $clone,
]);

// The pull makes the boot succeed, so the worker runs the entrypoint once. The cycle ends when the server stops.
try {
    $d->receive();
} catch (\Rapira\Exception\ClosedException) {
}
