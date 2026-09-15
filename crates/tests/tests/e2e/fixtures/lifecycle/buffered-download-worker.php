<?php

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;

$d = \Rapira\get_dispatcher();
$sequence = 0;
$result = null;
try {
    while (true) {
        $ex = $d->receive();
        ++$sequence;
        if ($ex->getRequest()->target === '/download') {
            // Larger than the client receive buffer plus the server send buffer, which autotune to a few MiB, so the write stalls in hyper.
            $size = 32 * 1024 * 1024;
            $ex->writeHead(200, ['content-length' => [(string) $size]]);
            $ex->writeBody(str_repeat('x', $size), eos: false);
            // Bounded, so a stuck test fails on the state assertion instead of a read timeout.
            $deadline = microtime(true) + 20;
            while (!$ex->isCancelled() && microtime(true) < $deadline) {
                usleep(1_000);
            }
            try {
                $ex->writeBody('', eos: true);
                $result = 'timeout';
            } catch (WorkDiscardedException) {
                $result = 'discarded';
            }
            continue;
        }
        $ex->writeBody(getmypid() . ":{$sequence}:{$result}");
    }
} catch (ClosedException) {
}
