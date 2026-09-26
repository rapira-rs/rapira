<?php

// Probe toggles ride on the request target: this mode has no superglobals.

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        if (isset($q['spin'])) {
            // never finalizes: the per-unit budget must kill it (timeout_tests.rs)
            while (true) {
            }
        }
        if (isset($q['limit'])) {
            set_time_limit((int) $q['limit']);
        }
        // burn=N uses N ms of CPU, which ITIMER_PROF counts against the budget
        $end = hrtime(true) + (int) ($q['burn'] ?? 0) * 1_000_000;
        while (hrtime(true) < $end) {
        }
        // pending=1 shows that the next unit is queued before the next receive()
        $ex->writeBody('pending=' . $d->getInfo()->pendingCount());
    }
} catch (\Rapira\Exception\ClosedException) {
}
