<?php
// handle_request() must refuse dispatcher mode before it touches the shared intake. The units served after that prove that the refusal was clean.

use Rapira\Exception\ClosedException;
use Rapira\Exception\NotInWorkerModeError;

try {
    \Rapira\handle_request(static function (): void {});
} catch (NotInWorkerModeError $e) {
    \Rapira\log('gate ' . $e::class);
}

try {
    rapira_finish_request();
} catch (\Error $e) {
    \Rapira\log('finish-gate');
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
