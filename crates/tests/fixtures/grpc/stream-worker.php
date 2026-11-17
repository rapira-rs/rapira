<?php

// The application side of the server-streaming tests. The request is an EchoRequest, and its text selects the probe. A reply is an EchoResponse, except in the flood probe.

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Grpc\ServerStreamingCall;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;

// EchoResponse { text: $text }, for a text shorter than 128 bytes.
function reply(string $text): string
{
    return "\x0a" . chr(strlen($text)) . $text;
}

function tail(): \Generator
{
    yield reply('2');
    yield reply('3');
}

// 1, then 2 and 3 from a delegate generator.
function count_to_three(): \Generator
{
    yield reply('1');
    yield from tail();
}

// Waits $startUs, yields 1, then yields one x each 10 ms. It ends when $catches yields threw because the host closed the call.
function slow(int $startUs, int $catches): \Generator
{
    try {
        usleep($startUs);
        for ($i = 0; $i < 1000; $i++) {
            try {
                yield reply($i === 0 ? '1' : 'x');
            } catch (WorkDiscardedException $e) {
                \Rapira\log('discarded', context: ['result' => $e::class]);
                if (--$catches === 0) {
                    return;
                }
            }
            usleep(10_000);
        }
    } finally {
        \Rapira\log('finally');
    }
}

function serve(ServerStreamingCall $call): void
{
    $md = $call->getResponseMetadata();
    $probe = substr($call->getMessage(), 2);
    switch ($probe) {
        case 'count':
            $call->respond(count_to_three());
            return;
        case 'meta':
            $md->addHeader('x-h', 'v');
            $md->addTrailer('x-t', 'w');
            $call->respond((function () use ($md) {
                $md->addHeader('x-gen', 'v');
                yield reply('1');
                try {
                    $md->addHeader('x-late', 'v');
                    $late = 'none';
                } catch (\Throwable $e) {
                    $late = $e::class;
                }
                \Rapira\log('late-header', context: ['result' => $late]);
                $md->addTrailer('x-late-t', 'w');
                yield reply('2');
            })());
            return;
        case 'empty':
            $md->addHeader('x-h', 'v');
            $md->addTrailer('x-t', 'w');
            // a generator that ends with no yield
            $call->respond((function () {
                yield from [];
            })());
            return;
        case 'fail-first':
            $md->addHeader('x-h', 'v');
            $md->addTrailer('x-t', 'w');
            $call->fail(new Status(StatusCode::NotFound, 'no invoice'));
            return;
        case 'throw':
            $md->addTrailer('x-t', 'w');
            try {
                $call->respond((function () use ($md) {
                    yield reply('1');
                    yield reply('2');
                    $md->addTrailer('x-late-t', 'w');
                    throw new \RuntimeException('boom');
                })());
            } catch (\RuntimeException $e) {
                \Rapira\log('thrown', context: [
                    'class' => $e::class,
                    'message' => $e->getMessage(),
                    'finalized' => $call->isFinalized(),
                ]);
            }
            $call->fail(new Status(StatusCode::NotFound, 'no invoice'));
            return;
        case 'bad-yield':
            try {
                $call->respond((function () {
                    yield reply('1');
                    yield 42;
                })());
            } catch (\TypeError $e) {
                \Rapira\log('bad-yield', context: ['result' => $e::class]);
            }
            $call->fail(new Status(StatusCode::Internal, 'bad yield'));
            return;
        case 'flood':
            // 200 messages of 64 KiB: more than the flow-control window of the client
            try {
                $call->respond((function () {
                    for ($i = 0; $i < 200; $i++) {
                        yield str_repeat(chr($i), 65536);
                    }
                })());
            } catch (WorkDiscardedException) {
                // the host closed the call while the generator waited at a yield
            }
            \Rapira\log('returned', context: [
                'finalized' => $call->isFinalized(),
                'cancelled' => $call->isCancelled(),
            ]);
            return;
        case 'garbage':
            // the second message is no EchoResponse: 0xff starts a field tag that does not end
            $call->respond((function () {
                yield reply('1');
                yield "\xff";
            })());
            return;
        case 'drop':
            // no outcome: the call is lost when its object goes
            return;
        case 'fatal':
            $call->respond((function () {
                yield reply('1');
                yield reply('2');
                eval('function reply() {}'); // a second declaration: a fatal error, and the engine bails out
            })());
            return;
        case 'slow':
        case 'slow-start':
        case 'stubborn':
            $call->respond(slow($probe === 'slow-start' ? 400_000 : 0, $probe === 'stubborn' ? 3 : 1));
            \Rapira\log('returned', context: [
                'finalized' => $call->isFinalized(),
                'cancelled' => $call->isCancelled(),
            ]);
            return;
        case 'late':
            // the deadline of the call passes during the sleep
            usleep(400_000);
            try {
                $call->respond((function () {
                    \Rapira\log('started');
                    yield reply('1');
                })());
                $late = 'none';
            } catch (WorkDiscardedException $e) {
                $late = $e::class;
            }
            \Rapira\log('late', context: ['result' => $late]);
            return;
    }
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        serve($d->receive());
    }
} catch (ClosedException) {
    // Every intake clone is gone.
}
