<?php
// Fails its boot while another worker holds boot.lock next to it: a per-worker resource that only one worker gets.

use Rapira\Exception\ClosedException;

$lock = fopen(__DIR__ . '/boot.lock', 'c');
if (!flock($lock, LOCK_EX | LOCK_NB)) {
    throw new RuntimeException('boot lock held');
}
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
