<?php

// wait=N sleeps N ms, then the unit streams 32 MiB, so a client that does not read parks a write on the full frame channel. burn=N then uses N ms of process CPU time, which ITIMER_PROF counts against the budget.

function cpu_ms(): float
{
    $u = getrusage();
    return ($u['ru_utime.tv_sec'] + $u['ru_stime.tv_sec']) * 1000
        + ($u['ru_utime.tv_usec'] + $u['ru_stime.tv_usec']) / 1000;
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        usleep((int) $q['wait'] * 1000);
        $chunk = str_repeat('x', 256 << 10);
        for ($i = 0; $i < 128; $i++) {
            $ex->writeBody($chunk, eos: false);
        }
        $end = cpu_ms() + (int) $q['burn'];
        while (cpu_ms() < $end) {
        }
        $ex->writeBody('done', eos: true);
    }
} catch (\Rapira\Exception\ClosedException) {
}
