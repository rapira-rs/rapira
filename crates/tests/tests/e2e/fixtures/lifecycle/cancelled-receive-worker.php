<?php

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
$method = 'receive';
$sequence = 0;
try {
    while (true) {
        $ex = $method === 'tryReceive' ? $d->tryReceive() : $d->receive();
        if ($ex === null) {
            usleep(1_000);
            continue;
        }
        ++$sequence;
        $target = $ex->getRequest()->target;
        if (parse_url($target, PHP_URL_PATH) === '/cancel') {
            parse_str(parse_url($target, PHP_URL_QUERY) ?: '', $q);
            $method = $q['method'] ?? 'receive';
            $ex->writeBody(getmypid() . ":{$sequence}:initial\n", eos: false);
            while (!$ex->isCancelled()) {
                usleep(1_000);
            }
            \Rapira\log('cancellation-observed');
            continue;
        }
        \Rapira\log('receive-result:ok');
        $ex->writeBody(getmypid() . ":{$sequence}");
    }
} catch (ClosedException) {
} catch (\Error $e) {
    \Rapira\log('receive-result:error:' . $e->getMessage());
}
