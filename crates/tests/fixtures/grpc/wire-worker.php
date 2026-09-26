<?php

// The application side of the wire tests. Each call logs a `call` record with its request facts, then the request metadata selects the outcome:
// x-halves: the response carries the header x-h: v and the trailers x-t: w and x-b-bin 01 02.
// x-fail: the call fails with NOT_FOUND `no invoice` and one detail of this type URL that packs 0a 01 78.
// x-reply: `ff` replies the byte ff, `many-ids` replies EchoResponse.ids with 600,000 elements of 1, `none` drops the call with no outcome.
// With no x-fail and no x-reply, the call replies with its request message.

use Rapira\Exception\ClosedException;
use Rapira\Grpc\ErrorDetail;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;
use Rapira\Grpc\UnaryCall;
use Rapira\InetAddress;

function serve(UnaryCall $call): void
{
    $ctx = $call->getContext();
    $metadata = [];
    foreach ($ctx->metadata as $name => $values) {
        $metadata[$name] = str_ends_with($name, '-bin') ? array_map(bin2hex(...), $values) : $values;
    }
    $r = $ctx->remote;
    \Rapira\log('call', context: [
        'method' => $ctx->method,
        'protocol' => $ctx->protocol->value,
        'metadata' => (object) $metadata,
        'deadline' => $ctx->deadline,
        'remote' => $r instanceof InetAddress ? "{$r->ip}:{$r->port}" : 'unix:' . var_export($r->path, true),
        'message' => bin2hex($call->getMessage()),
    ]);

    $in = $ctx->metadata;
    if ($in->values('x-halves') !== []) {
        $out = $call->getResponseMetadata();
        $out->addHeader('x-h', 'v');
        $out->addTrailer('x-t', 'w');
        $out->addBinaryTrailer('x-b-bin', "\x01\x02");
    }
    $fail = $in->values('x-fail');
    if ($fail !== []) {
        $call->fail(new Status(StatusCode::NotFound, 'no invoice', [new ErrorDetail($fail[0], "\x0a\x01x")]));
        return;
    }
    switch ($in->values('x-reply')[0] ?? '') {
        case 'ff':
            $call->respond("\xff");
            return;
        case 'many-ids':
            $call->respond("\x1a\xc0\xcf\x24" . str_repeat("\x01", 600_000));
            return;
        case 'none':
            return; // never finalized: the plugin loses this call
    }
    $call->respond($call->getMessage());
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        serve($d->receive());
    }
} catch (ClosedException) {
    // Every intake clone is gone.
    \Rapira\log('closed');
}
