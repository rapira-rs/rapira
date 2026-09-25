<?php

// Serves rapira.test.v1.EchoService. The `text` field of EchoRequest selects the probe:
// for a text shorter than 128 bytes the message is `0a <length> <text>`.

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Grpc\UnaryCall;

function serve(UnaryCall $call): void
{
    $m = $call->getMessage();
    switch (substr($m, 2)) {
        case 'slow':
            for ($i = 0; $i < 500 && !$call->isCancelled(); $i++) {
                usleep(10_000);
            }
            $cancelled = $call->isCancelled() ? 'yes' : 'no';
            try {
                $call->respond($m);
                $respond = 'none';
            } catch (WorkDiscardedException $e) {
                $respond = $e::class;
            }
            \Rapira\log('slow', context: ['cancelled' => $cancelled, 'respond' => $respond]);
            return;
        case 'slow-ok':
            \Rapira\log('slow started');
            usleep(500_000);
            break;
        case 'meta':
            $in = $call->getContext()->metadata;
            \Rapira\log('meta', context: ['keys' => array_keys($in->entries)]);
            $out = $call->getResponseMetadata();
            foreach ($in->values('x-echo') as $value) {
                $out->addHeader('x-echo', $value);
            }
            foreach ($in->values('x-echo-bin') as $bytes) {
                $out->addBinaryTrailer('x-echo-bin', $bytes);
            }
            $call->respond($m);
            return;
    }
    $call->respond($m);
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        serve($d->receive());
    }
} catch (ClosedException) {
}
