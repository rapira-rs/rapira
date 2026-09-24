<?php

// The message bytes select the probe: a gRPC call has no request target.

use Rapira\Exception\AlreadyFinalizedError;
use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Grpc\ErrorDetail;
use Rapira\Grpc\GrpcDispatcher;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;
use Rapira\Grpc\UnaryCall;
use Rapira\InetAddress;

// Logs `$name` with the return value of `$run`, or with the class of what it threw.
function probe(string $name, callable $run): void
{
    try {
        $result = $run();
    } catch (\Throwable $e) {
        $result = $e::class;
    }
    \Rapira\log('case', context: ['name' => $name, 'result' => $result]);
}

function serve(GrpcDispatcher $d, UnaryCall $call): void
{
    $msg = $call->getMessage();

    if (str_starts_with($msg, 'echo:')) {
        $text = substr($msg, 5);
        \Rapira\log('echo', context: ['result' => $text]);
        $call->respond($text);
        return;
    }
    if (str_starts_with($msg, 'hold:')) {
        \Rapira\log('held');
        $marker = substr($msg, 5);
        for ($i = 0; $i < 1000 && !file_exists($marker); $i++) {
            usleep(10_000);
        }
        $call->respond('held');
        return;
    }

    switch ($msg) {
        case 'fail':
            $call->fail(new Status(
                StatusCode::NotFound,
                'no invoice',
                [new ErrorDetail('type.googleapis.com/google.rpc.ErrorInfo', "\x0a\x01x")],
            ));
            return;
        case 'bad-fail':
            try {
                $call->fail(new Status(StatusCode::Internal, 'x', ['not a detail']));
            } catch (\TypeError $e) {
                \Rapira\log('bad-fail', context: ['result' => $e::class]);
            }
            $call->respond('recovered');
            return;
        case 'twice':
            $call->respond('a');
            try {
                $call->respond('b');
            } catch (AlreadyFinalizedError $e) {
                \Rapira\log('twice', context: ['result' => $e::class . ': ' . $e->getMessage()]);
            }
            return;
        case 'busy':
            try {
                $d->receive(0);
            } catch (\Error $e) {
                \Rapira\log('busy', context: ['result' => $e->getMessage()]);
            }
            $call->respond('busy');
            return;
        case 'drop':
            unset($call); // never finalized: the host loses this call
            return;
        case 'throw':
            throw new \RuntimeException('uncaught');
        case 'info':
            $call->respond((string) $d->getInfo()->activeCount());
            return;
        case 'context':
            $ctx = $call->getContext();
            $metadata = [];
            foreach ($ctx->metadata as $name => $values) {
                $metadata[$name] = str_ends_with($name, '-bin') ? array_map(bin2hex(...), $values) : $values;
            }
            $r = $ctx->remote;
            \Rapira\log('context', context: [
                'same' => $ctx === $call->getContext(),
                'class' => $ctx::class,
                'method' => $ctx->method,
                'metadata' => (object) $metadata,
                'deadline' => $ctx->deadline,
                'remote' => $r instanceof InetAddress ? "{$r->ip}:{$r->port}" : 'unix:' . var_export($r->path, true),
                'tls' => $ctx->tls,
                'protocol' => $ctx->protocol->value,
                'receivedAt' => $ctx->receivedAt,
            ]);
            $call->respond('context');
            return;
        case 'meta':
            $md = $call->getResponseMetadata();
            $md->addHeader('x-h', 'v');
            $md->addBinaryHeader('x-b-bin', "\x01\x02");
            $md->addTrailer('x-t', 'w');
            $call->respond('meta');
            return;
        case 'md-rules':
            $md = $call->getResponseMetadata();
            probe('the accumulator is memoized', fn () => var_export($md === $call->getResponseMetadata(), true));
            probe('an upper-case name is normalized', static function () use ($md): string {
                $md->addHeader('X-Up', 'v');
                return implode(',', array_keys($md->headers()->entries));
            });
            probe('a reserved name', fn () => $md->addHeader('grpc-status', '0'));
            probe('-bin on addHeader', fn () => $md->addHeader('x-t-bin', 'a'));
            probe('headers() holds raw bytes', static function () use ($md): string {
                $md->addBinaryHeader('x-raw-bin', "\x01\x02");
                return bin2hex($md->headers()->values('x-raw-bin')[0]);
            });
            $call->respond('md-rules');
            probe('add after respond', fn () => $md->addHeader('x-late', 'v'));
            unset($call);
            probe('add after the call object is gone', fn () => $md->addHeader('x-gone', 'v'));
            probe('headers() after the call object is gone', fn () => json_encode($md->headers()->entries));
            return;
        case 'cancel':
            \Rapira\log('got'); // the call is out; the test may drop the receiver
            for ($i = 0; $i < 1000 && !$call->isCancelled(); $i++) {
                usleep(10_000);
            }
            $state = ['cancelled' => $call->isCancelled(), 'finalized' => $call->isFinalized()];
            try {
                $call->respond('late');
                $state['respond'] = 'none';
            } catch (WorkDiscardedException $e) {
                $state['respond'] = $e::class . ': ' . $e->getMessage();
            }
            \Rapira\log('cancel', context: $state);
            return;
    }
    $call->respond('unknown probe');
}

$d = \Rapira\get_dispatcher();

// A call queued before the boot comes out of tryReceive(); the tests wait for this record before they call.
$first = $d->tryReceive();
\Rapira\log('try', context: ['result' => $first === null ? 'NULL' : $first::class]);
if ($first !== null) {
    serve($d, $first);
    unset($first);
}

try {
    while (true) {
        serve($d, $d->receive());
    }
} catch (ClosedException) {
}
