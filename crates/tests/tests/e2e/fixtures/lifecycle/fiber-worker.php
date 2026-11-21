<?php
// Each unit runs inside its own Fiber, with a suspend point between the read of the request and the write of the answer. The correlation must stay correct across the suspension.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $fiber = new Fiber(static function () use ($ex): void {
            parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
            $n = (int) ($q['n'] ?? 0);
            Fiber::suspend();
            $ex->writeBody('r=' . ($n + 1));
        });
        $fiber->start();
        $fiber->resume();
    }
} catch (ClosedException) {
}
