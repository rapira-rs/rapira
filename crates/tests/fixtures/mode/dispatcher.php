<?php

$mode = \Rapira\get_mode();

// No response to write into here, so the results travel out through the app log.
// A pure enum is not json-encodable, so the context carries the name and the comparisons only.
\Rapira\log('mode', context: [
    'name' => $mode->name,
    'case' => $mode === \Rapira\Mode::Dispatcher,
    'class' => $mode::class,
]);

// The pull makes the boot succeed, so the worker runs the script once. The script ends when the server stops.
try {
    \Rapira\get_dispatcher()->receive();
} catch (\Rapira\Exception\ClosedException) {
}
