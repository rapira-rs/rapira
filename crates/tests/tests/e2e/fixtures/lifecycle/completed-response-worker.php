<?php

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;

$d = \Rapira\get_dispatcher();
$sequence = 0;
$result = null;
$acknowledgement = __DIR__ . '/client-read';
try {
    while (true) {
        $ex = $d->receive();
        ++$sequence;
        $path = $ex->getRequest()->target;
        if ($path === '/state') {
            $ex->writeBody(getmypid() . ":{$sequence}:{$result}");
            continue;
        }
        $length = match ($path) {
            '/body' => 5,
            '/empty' => 0,
            '/file' => filesize(__DIR__ . '/payload.bin'),
            default => throw new RuntimeException("unexpected target {$path}"),
        };
        $ex->writeHead(200, ['content-length' => [(string) $length]]);
        if ($path === '/file') {
            $ex->sendFile(__DIR__ . '/payload.bin', eos: false);
        } elseif ($length > 0) {
            $ex->writeBody('01234', eos: false);
        } else {
            $ex->flush();
        }
        // Bounded, so a stuck test fails on the state assertion instead of a read timeout.
        $deadline = microtime(true) + 20;
        while (!file_exists($acknowledgement) && microtime(true) < $deadline) {
            usleep(1_000);
            clearstatcache(true, $acknowledgement);
        }
        try {
            $ex->writeBody('', eos: true);
            $result = file_exists($acknowledgement) ? 'finalized' : 'timeout';
        } catch (WorkDiscardedException) {
            $result = 'discarded';
        }
    }
} catch (ClosedException) {
}
