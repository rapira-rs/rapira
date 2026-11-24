<?php

// The request target carries the probe toggles, because this mode has no superglobals.

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        if (isset($q['spin'])) {
            // this unit is never finalized, so the per-unit budget must kill it (timeout_tests.rs)
            while (true) {
            }
        }
        if (isset($q['limit'])) {
            set_time_limit((int) $q['limit']);
        }
        // Sleep for N ms. A sleep uses wall-clock time and no CPU time.
        usleep((int) ($q['sleep'] ?? 0) * 1000);
        // Spin for N ms of wall-clock time. The loop also uses CPU time.
        $end = hrtime(true) + (int) ($q['burn'] ?? 0) * 1_000_000;
        while (hrtime(true) < $end) {
        }
        // pending=1 shows that the next unit is queued before the next receive()
        $ex->writeBody('pending=' . $d->getInfo()->pendingCount());
    }
} catch (\Rapira\Exception\ClosedException) {
}
