<?php

use Rapira\Exception\ClosedException;

// The dispatcher twin of worker/held-worker.php: counts the units of this app instance and holds each one until the release file exists.
$runs = 0;
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        if (($q['probe'] ?? '') === 'count') {
            $ex->writeBody("runs=$runs");
            continue;
        }
        $runs++;
        \Rapira\log('held');
        while (!file_exists($q['release'])) {
            usleep(10000);
        }
        $ex->writeBody('done');
    }
} catch (ClosedException) {
}
