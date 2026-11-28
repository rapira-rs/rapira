<?php

require __DIR__ . '/view.php';

if (\Rapira\get_mode() === \Rapira\Mode::Worker) {
    $handler = static function (): void {
        echo pool_view();
    };
    while (\Rapira\handle_request($handler)) {
    }
    return;
}

$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $exchange->writeBody(pool_view($dispatcher));
    }
} catch (\Rapira\Exception\ClosedException) {
}
