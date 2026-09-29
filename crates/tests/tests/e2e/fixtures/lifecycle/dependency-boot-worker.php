<?php
// Fails its boot until up.flag exists next to it: a dependency that is down at the start and comes up later.

use Rapira\Exception\ClosedException;

if (!file_exists(__DIR__ . '/up.flag')) {
    throw new RuntimeException('dependency down');
}
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
