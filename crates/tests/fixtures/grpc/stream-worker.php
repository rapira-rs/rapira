<?php

// The app side of the server-streaming tests. The request is an EchoRequest, and its text selects the probe. A reply is an EchoResponse, except in the flood probe, the first message of the stall probe and the second message of the garbage probe.

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

// Yields 1, then yields 2 and 3 from a delegate generator.
function count_to_three(): \Generator
{
    yield reply('1');
    yield from tail();
}

// Waits $startUs microseconds, yields 1, then yields one x each 10 ms. It returns after $catches yields throw because the host closed the call.
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
            // the generator ends with no yield
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
        case 'fail-inside':
            $call->respond((function () use ($call) {
                yield reply('1');
                $call->fail(new Status(StatusCode::NotFound, 'no invoice'));
                try {
                    yield reply('2');
                    $next = 'none';
                } catch (\Throwable $e) {
                    $next = $e::class;
                }
                \Rapira\log('after-fail', context: ['result' => $next]);
            })());
            return;
        case 'twice':
            $call->respond(count_to_three());
            try {
                $call->respond(count_to_three());
                $second = 'none';
            } catch (\Throwable $e) {
                $second = $e::class;
            }
            \Rapira\log('second-respond', context: ['result' => $second]);
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
            // 200 messages of 64 KiB are more than the flow-control window of the client
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
        case 'stall':
            $call->respond((function () use ($call) {
                // 8 MiB is more than the 5 MiB connection flow-control window of the test client, which is DEFAULT_CONN_WINDOW of the hyper 1.12 HTTP/2 client. So the connection takes no other message while the client does not read
                yield str_repeat('x', 8 * 1024 * 1024);
                // hyper takes this message from the body and holds it until the window has room, so it polls the body no more
                yield reply('1');
                // the deadline of the call passes during the sleep
                usleep(600_000);
                \Rapira\log('expired', context: [
                    'finalized' => $call->isFinalized(),
                    'cancelled' => $call->isCancelled(),
                ]);
                try {
                    yield reply('2');
                    $next = 'none';
                } catch (WorkDiscardedException $e) {
                    $next = $e::class;
                }
                \Rapira\log('next-yield', context: ['result' => $next]);
            })());
            return;
        case 'garbage':
            // the second message is not an EchoResponse, because 0xff starts a field tag that does not end
            $call->respond((function () {
                yield reply('1');
                yield "\xff";
            })());
            return;
        case 'drop':
            // the call gets no outcome, so it is lost when its object is released
            return;
        case 'fatal':
            $call->respond((function () {
                yield reply('1');
                yield reply('2');
                eval('function reply() {}'); // a second declaration is a fatal error, and the engine bails out
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
