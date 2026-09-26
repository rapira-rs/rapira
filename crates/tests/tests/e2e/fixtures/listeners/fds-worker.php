<?php
// Resident worker: each response lists the descriptors of a child process that the worker starts.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody((string) shell_exec('ls -l /proc/self/fd 2>&1'));
    }
} catch (ClosedException) {
}
