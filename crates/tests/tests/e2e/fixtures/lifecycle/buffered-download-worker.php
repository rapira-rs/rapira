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
            $size = 32 * 1024 * 1024;
            $ex->writeHead(200, ['content-length' => [(string) $size]]);
            $ex->writeBody(str_repeat('x', $size), eos: false);
            while (!$ex->isCancelled()) {
                usleep(1_000);
            }
            try {
                $ex->writeBody('', eos: true);
                $result = 'finalized';
            } catch (WorkDiscardedException) {
                $result = 'discarded';
            }
            continue;
        }
        $ex->writeBody(getmypid() . ":{$sequence}:{$result}");
    }
} catch (ClosedException) {
}
