<?php

try {
    $exchange = Rapira\get_dispatcher()->receive();
} catch (Rapira\Exception\ClosedException) {
    return;
}
$request = $exchange->getRequest();
$stored = $request->traceContext;
$fatal = $request->target === '/fatal';
register_shutdown_function(static function () use ($exchange, $stored, $fatal): void {
    $active = Rapira\trace_context();
    Rapira\log('dispatcher-shutdown-start');
    $exchange->writeHead($fatal ? 500 : 200);
    $exchange->writeBody(json_encode(['active' => $active, 'stored' => $stored], JSON_THROW_ON_ERROR));
    Rapira\log('dispatcher-shutdown-finished', context: ['carrier' => Rapira\trace_context()]);
});
if ($fatal) {
    trigger_error('dispatcher fatal before shutdown', E_USER_ERROR);
}
