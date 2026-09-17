<?php

$dispatcher = Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $request = $exchange->getRequest();
        Rapira\log('request handled');
        $exchange->writeHead(200, ['content-type' => ['application/json']]);
        $exchange->writeBody(json_encode([
            'pid' => getmypid(),
            'traceContext' => $request->traceContext,
        ], JSON_THROW_ON_ERROR));
    }
} catch (Rapira\Exception\ClosedException) {
}
