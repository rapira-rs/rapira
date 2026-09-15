<?php

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
$sequence = 0;
$result = null;
try {
    while (true) {
        $ex = $d->receive();
        ++$sequence;
        if ($ex->getRequest()->target === '/events') {
            $ex->writeHead(200, ['content-type' => ['text/event-stream']]);
            $ex->writeBody("data: connected\n\n", eos: false);
            // Bounded, so a stuck test fails on the state assertion instead of a read timeout.
            $deadline = microtime(true) + 20;
            while (!$ex->isCancelled() && microtime(true) < $deadline) {
                usleep(1_000);
            }
            if ($ex->isCancelled()) {
                $result = 'cancelled';
            } else {
                $result = 'timeout';
                $ex->writeBody('', eos: true);
            }
            continue;
        }
        $ex->writeBody(getmypid() . ":{$sequence}:{$result}");
    }
} catch (ClosedException) {
}
