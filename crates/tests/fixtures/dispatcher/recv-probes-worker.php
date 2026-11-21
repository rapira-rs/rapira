<?php

// Probes an empty intake. The test never creates a handle, so no unit can come before the probes.

use Rapira\Exception\TimeoutException;

$d = \Rapira\get_dispatcher();
$probes = [
    'try' => $d->tryReceive() === null ? 'null' : 'unit',
];
try {
    $d->receive(0);
    $probes['zero'] = 'unit';
} catch (TimeoutException) {
    $probes['zero'] = 'timeout';
}
try {
    $d->receive(50_000);
    $probes['short'] = 'unit';
} catch (TimeoutException) {
    $probes['short'] = 'timeout';
}
\Rapira\log('recv-probes', context: $probes);
