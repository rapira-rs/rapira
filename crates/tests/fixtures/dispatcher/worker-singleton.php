<?php

$d = \Rapira\get_dispatcher();

try {
    clone $d;
    $clone = 'allowed';
} catch (\Error) {
    $clone = 'blocked';
}

// No response to write into here, so the results travel out through the app log.
\Rapira\log('dispatcher', context: [
    'class' => $d::class,
    'name' => $d->name(),
    'same' => $d === \Rapira\get_dispatcher(),
    'http' => $d instanceof \Rapira\Http\HttpDispatcher,
    'base' => $d instanceof \Rapira\Dispatcher,
    'clone' => $clone,
]);

// The pull makes the boot succeed, so the worker runs the script once. The script ends when the server stops.
try {
    $d->receive();
} catch (\Rapira\Exception\ClosedException) {
}
