<?php
$boot = require __DIR__ . '/values.php';
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $d->receive()->writeBody($boot);
    }
} catch (\Rapira\Exception\ClosedException) {
}
