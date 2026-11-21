<?php

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        // in the php log target, E_USER_WARNING logs at warn and E_USER_NOTICE at info
        trigger_error('WARN-MARK diagnostic', E_USER_WARNING);
        trigger_error('NOTICE-MARK diagnostic', E_USER_NOTICE);
        \Rapira\log('APP-MARK', \Rapira\LogLevel::Info, ['answer' => 42]);
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
