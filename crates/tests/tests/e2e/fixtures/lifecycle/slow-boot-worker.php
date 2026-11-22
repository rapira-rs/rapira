<?php
// Wait for the test before the first pull.

use Rapira\Exception\ClosedException;

while (!is_file(__DIR__ . '/release-boot')) {
    usleep(10_000);
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
