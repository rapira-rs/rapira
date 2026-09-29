<?php
// The boot fails while another worker holds boot.lock in this directory. Only one worker at a time can hold the lock.

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
