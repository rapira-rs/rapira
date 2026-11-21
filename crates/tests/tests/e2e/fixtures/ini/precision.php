<?php

// precision is a plain scalar ini setting with a known built-in default of 14, so the body shows whether PHP applies the php.ini that the test writes.

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeBody('precision=' . ini_get('precision'));
    }
} catch (\Rapira\Exception\ClosedException) {
}
