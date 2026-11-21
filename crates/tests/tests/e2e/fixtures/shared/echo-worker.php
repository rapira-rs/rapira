<?php
// A resident worker. Each response holds the pid of the worker that serves it, so the tests can check continuity.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok:' . getmypid());
    }
} catch (ClosedException) {
}
