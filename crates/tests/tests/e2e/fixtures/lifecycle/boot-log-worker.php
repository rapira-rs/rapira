<?php
// Log the start and end of each app instance.

use Rapira\Exception\ClosedException;

\Rapira\log('booted');
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
    \Rapira\log('app loop ended');
}
