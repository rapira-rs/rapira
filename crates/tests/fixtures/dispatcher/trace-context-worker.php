<?php

$outside = \Rapira\trace_context();
$previousExchange = null;
$previousRequest = null;
$turn = 0;
$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $active = \Rapira\trace_context();
        $request = ++$turn === 1 ? null : $exchange->getRequest();
        $previousLazy = $previousExchange?->getRequest()->traceContext;
        $previous = $previousRequest?->traceContext;
        $exchange->writeBody(json_encode([
            'outside' => $outside,
            'active' => $active,
            'request' => $request?->traceContext,
            'cached' => $request === null ? null : $request === $exchange->getRequest(),
            'previousLazy' => $previousLazy,
            'previous' => $previous,
        ]));
        $outside = \Rapira\trace_context();
        $previousExchange = $exchange;
        $previousRequest = $request;
    }
} catch (\Rapira\Exception\ClosedException) {
    \Rapira\log('trace-outside', context: ['carrier' => \Rapira\trace_context()]);
}
