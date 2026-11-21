<?php
// ?hang=1 blocks the request forever in native sleep, so the request_terminate_timeout watchdog of the master must kill the worker.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        if (($q['hang'] ?? '') === '1') {
            while (true) {
                usleep(100000);
            }
        }
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok:' . getmypid());
    }
} catch (ClosedException) {
}
