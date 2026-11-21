<?php
// Logs each boot and the end of each app instance. The tests use it to see each recycle inside one worker process.

use Rapira\Exception\ClosedException;

\Rapira\log('booted');
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody((string) getmypid());
    }
} catch (ClosedException) {
    \Rapira\log('app loop ended');
}
