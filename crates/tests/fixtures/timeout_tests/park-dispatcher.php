<?php

// The client stops reading while the unit writes 32 MiB. Use the same clock as PHP for the work on each side of the write.

function cpu_ms(): float
{
    $u = getrusage();
    return ($u['ru_utime.tv_sec'] + $u['ru_stime.tv_sec']) * 1000
        + ($u['ru_utime.tv_usec'] + $u['ru_stime.tv_usec']) / 1000;
}

function work(int $ms, string $clock): void
{
    if ($clock === 'wall') {
        usleep($ms * 1000);
        return;
    }
    $cpu = cpu_ms() + $ms;
    while (cpu_ms() < $cpu) {
    }
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        work((int) $q['before'], $q['clock']);
        $chunk = str_repeat('x', 256 << 10);
        for ($i = 0; $i < 128; $i++) {
            $ex->writeBody($chunk, eos: false);
        }
        work((int) $q['after'], $q['clock']);
        $ex->writeBody('done', eos: true);
    }
} catch (\Rapira\Exception\ClosedException) {
}
