<?php

$mode = \Rapira\get_mode();

// The boot has no unit to answer, so the results go to the app log.
// A pure enum is not json-encodable, so the context carries the name and the comparisons only.
\Rapira\log('mode', context: [
    'name' => $mode->name,
    'case' => $mode === \Rapira\Mode::Dispatcher,
    'class' => $mode::class,
]);

// The pull makes the boot succeed, so the worker runs the entrypoint once. The cycle ends when the server stops.
try {
    \Rapira\get_dispatcher()->receive();
} catch (\Rapira\Exception\ClosedException) {
}
