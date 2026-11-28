<?php

require __DIR__ . '/hold.php';

use Rapira\Grpc\ServerStreamingCall;

$app = bin2hex(random_bytes(16));
$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $call = $dispatcher->receive();
        $remote = $call->getContext()->remote;
        $call->getResponseMetadata()->addHeader('x-view', json_encode([
            'pid' => getmypid(), 'app' => $app, 'peer' => "{$remote->ip}:{$remote->port}",
        ], JSON_THROW_ON_ERROR));
        $message = $call->getMessage();
        if ($call instanceof ServerStreamingCall) {
            $call->respond((static function () use ($message): Generator {
                yield $message;
                hold_until_release();
                yield "\x0a\x08released";
            })());
        } else {
            $call->respond($message);
        }
    }
} catch (\Rapira\Exception\ClosedException) {
}
