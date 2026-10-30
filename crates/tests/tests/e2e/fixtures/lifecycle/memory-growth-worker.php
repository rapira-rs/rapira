<?php
// ?grow=N keeps N more MiB of written memory for the life of the process, then holds the request for 1.5 s: longer than one master tick.

use Rapira\Exception\ClosedException;

ini_set('memory_limit', '-1');
$kept = [];
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        if (isset($q['grow'])) {
            $kept[] = str_repeat('x', (int) $q['grow'] << 20);
            usleep(1_500_000);
        }
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
