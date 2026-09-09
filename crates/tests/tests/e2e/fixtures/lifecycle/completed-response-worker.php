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
        };
        $ex->writeHead(200, ['content-length' => [(string) $length]]);
        if ($path === '/file') {
            $ex->sendFile(__DIR__ . '/payload.bin', eos: false);
        } elseif ($length > 0) {
            $ex->writeBody('01234', eos: false);
        } else {
            $ex->flush();
        }
        while (!file_exists($acknowledgement)) {
            usleep(1_000);
            clearstatcache(true, $acknowledgement);
        }
        $cancelled = $ex->isCancelled();
        try {
            $ex->writeBody('', eos: true);
            $result = json_encode([$cancelled, $ex->isFinalized(), $ex->isCancelled()]);
        } catch (WorkDiscardedException $e) {
            $result = $e::class;
        }
    }
} catch (ClosedException) {
}
