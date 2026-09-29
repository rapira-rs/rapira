<?php
// Sleeps 3 s before its first receive(): the slot shows starting until the first pull.

use Rapira\Exception\ClosedException;

sleep(3);
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
