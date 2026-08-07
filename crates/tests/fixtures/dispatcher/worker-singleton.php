<?php

$d = \Rapira\get_dispatcher();

try {
    clone $d;
    $clone = 'allowed';
} catch (\Error) {
    $clone = 'blocked';
}

// Worker output has nowhere to go until the Exchange verbs land, so the
// results travel out through the app log.
\Rapira\log('dispatcher', context: [
    'class' => $d::class,
    'name' => $d->name(),
    'same' => $d === \Rapira\get_dispatcher(),
    'http' => $d instanceof \Rapira\Http\HttpDispatcher,
    'base' => $d instanceof \Rapira\Dispatcher,
    'clone' => $clone,
]);
