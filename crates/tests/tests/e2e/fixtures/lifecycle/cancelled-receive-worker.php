<?php

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
$sequence = 0;
try {
    while (true) {
        $ex = $d->receive();
        ++$sequence;
        if ($ex->getRequest()->target === '/events') {
            $ex->writeHead(200, ['content-type' => ['text/event-stream']]);
            $ex->writeBody("data: connected\n\n", eos: false);
            while (!$ex->isCancelled()) {
                usleep(1_000);
            }
            continue;
        }
        $ex->writeBody(getmypid() . ":{$sequence}");
    }
} catch (ClosedException) {
}
