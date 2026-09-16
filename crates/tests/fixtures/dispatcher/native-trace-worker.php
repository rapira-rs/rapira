<?php

$retained = [];
$dispatcher = \Rapira\get_dispatcher();
\Rapira\log('trace-idle');
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $request = $exchange->getRequest();
        \Rapira\log('trace-active');
        if ($request->target === '/throw') {
            throw new \RuntimeException('trace-throw');
        }
        if ($request->target === '/fatal') {
            trigger_error('trace-fatal', E_USER_ERROR);
        }
        if ($request->target === '/exit') {
            exit;
        }
        $exchange->writeBody('ok');
        $retained[] = [$exchange, $request];
        \Rapira\log('trace-idle');
    }
} catch (\Rapira\Exception\ClosedException) {
}
