<?php
// Each request returns the THP_enabled value of the serving worker from /proc/self/status.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        preg_match('/^THP_enabled:\s+(\d)$/m', file_get_contents('/proc/self/status'), $m);
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody($m[1] ?? 'missing');
    }
} catch (ClosedException) {
}
