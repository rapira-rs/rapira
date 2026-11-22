<?php

// The client stops reading while the unit writes 32 MiB. The work before and after the write uses both CPU time and wall-clock time.

function cpu_ms(): float
{
    $u = getrusage();
    return ($u['ru_utime.tv_sec'] + $u['ru_stime.tv_sec']) * 1000
        + ($u['ru_utime.tv_usec'] + $u['ru_stime.tv_usec']) / 1000;
}

function work(int $ms): void
{
    $cpu = cpu_ms() + $ms;
    $wall = hrtime(true) + $ms * 1_000_000;
    while (cpu_ms() < $cpu || hrtime(true) < $wall) {
    }
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        work((int) $q['before']);
        $chunk = str_repeat('x', 256 << 10);
        for ($i = 0; $i < 128; $i++) {
            $ex->writeBody($chunk, eos: false);
        }
        work((int) $q['after']);
        $ex->writeBody('done', eos: true);
    }
} catch (\Rapira\Exception\ClosedException) {
}
