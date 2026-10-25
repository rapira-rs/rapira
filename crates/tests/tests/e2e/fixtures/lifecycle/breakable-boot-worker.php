<?php
// Every boot throws while broken.flag exists next to it: a deploy that breaks the boot, reloaded in place.

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
