<?php
// Every boot throws while broken.flag exists next to this file. This simulates a deploy that breaks the boot and is reloaded in place.

use Rapira\Exception\ClosedException;

if (file_exists(__DIR__ . '/broken.flag')) {
    throw new RuntimeException('broken deploy');
}
\Rapira\log('booted');
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
