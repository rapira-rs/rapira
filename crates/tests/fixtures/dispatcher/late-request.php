<?php

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $first = $d->receive();
        $first->writeBody("sealed\n");
        $next = $d->receive();
        $request = $first->getRequest();
        $next->writeBody(json_encode([
            'final' => $first->isFinalized(),
            'same' => $request === $first->getRequest(),
            'method' => $request->method,
            'target' => $request->target,
            'uri' => $request->uri,
            'length' => strlen($request->body),
            'sha256' => hash('sha256', $request->body),
        ], JSON_THROW_ON_ERROR));
    }
} catch (\Rapira\Exception\ClosedException) {
}
