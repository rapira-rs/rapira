<?php
// The boot fails while another worker holds boot.lock in this directory. Only one worker at a time can hold the lock.

use Rapira\Exception\ClosedException;

$lock = fopen(__DIR__ . '/boot.lock', 'c');
if (!flock($lock, LOCK_EX | LOCK_NB)) {
    \Rapira\log('boot lock held');
    return;
}
if (getenv('HOLD_BOOT') === '1') {
    \Rapira\log('waiting at boot');
    sleep(30);
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
