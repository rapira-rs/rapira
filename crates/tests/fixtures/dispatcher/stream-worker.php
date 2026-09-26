<?php

// Probe toggles ride on the request target: this mode has no superglobals.

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Http\Exception\ContentLengthExceededError;
use Rapira\Http\Exception\FileNotSendableException;
use Rapira\Http\Exception\HeadNotWrittenError;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        parse_str(parse_url($req->target, PHP_URL_QUERY) ?: '', $q);
        $probe = $q['probe'] ?? '';

        if ($probe === 'flush-park') {
            $ex->writeHead(200, ['x-probe' => ['flush-park']]);
            $ex->flush();
            usleep(300_000);
            $ex->writeBody('after');
            continue;
        }
        if ($probe === 'chunks') {
            $ex->writeBody('one,', eos: false);
            $ex->writeBody('two,', eos: false);
            $ex->writeBody('three', eos: true);
            continue;
        }
        if ($probe === 'cl-exceeded') {
            $ex->writeHead(200, ['content-length' => ['5']]);
            try {
                $ex->writeBody('0123456789');
            } catch (ContentLengthExceededError $e) {
                \Rapira\log('cl-exceeded', context: ['class' => $e::class]);
            }
            continue;
        }
        if ($probe === 'discard') {
            \Rapira\log('discard-held'); // the unit is out; the test may drop the receiver
            usleep(200_000);
            try {
                $ex->writeBody('into the void', eos: false);
                \Rapira\log('discard', context: ['class' => 'none']);
            } catch (WorkDiscardedException $e) {
                \Rapira\log('discard', context: [
                    'class' => $e::class,
                    'cancelled' => $ex->isCancelled(),
                    'finalized' => $ex->isFinalized(),
                ]);
            }
            continue;
        }
        if ($probe === 'sendfile') {
            $ex->sendFile($req->headers['x-path'][0] ?? '');
            continue;
        }
        if ($probe === 'sendfile-slice') {
            $ex->writeHead(206, ['content-range' => ['bytes 2-4/26']]);
            $ex->sendFile($req->headers['x-path'][0] ?? '', 2, 3);
            continue;
        }
        if ($probe === 'sendfile-range') {
            $length = isset($q['length']) ? (int) $q['length'] : null;
            try {
                $ex->sendFile($req->headers['x-path'][0] ?? '', (int) ($q['offset'] ?? 0), $length);
            } catch (FileNotSendableException) {
                $ex->writeHead(403);
                $ex->writeBody('denied');
            }
            continue;
        }
        if ($probe === 'sendfile-shrink') {
            // The flushed head has no content-length, so the body is chunked and only a cut body lacks the last chunk.
            // The marker tells the test that the file is cut.
            $path = $req->headers['x-path'][0] ?? '';
            $ex->flush();
            $ex->sendFile($path);
            $f = fopen($path, 'r+');
            ftruncate($f, (int) ($q['to'] ?? 0));
            fclose($f);
            touch($path . '.shrunk');
            continue;
        }
        if ($probe === 'sendfile-missing') {
            try {
                $ex->sendFile('/definitely/not/here');
            } catch (FileNotSendableException) {
                // raised before anything was written: a 404 is still possible
                $ex->writeHead(404);
                $ex->writeBody('nope');
            }
            continue;
        }
        if ($probe === 'sendfile-escape') {
            try {
                $ex->sendFile($req->headers['x-path'][0] ?? '');
            } catch (FileNotSendableException) {
                $ex->writeHead(403);
                $ex->writeBody('denied');
            }
            continue;
        }
        if ($probe === 'trailers') {
            $ex->writeBody('chunk,', eos: false);
            $ex->writeTrailers(['x-checksum' => ['abc123']]);
            continue;
        }
        if ($probe === 'trailers-only') {
            $ex->writeHead(200, ['x-kind' => ['trailers-only']]);
            $ex->writeTrailers(['x-checksum' => ['empty']]);
            continue;
        }
        if ($probe === 'trailers-no-head') {
            try {
                $ex->writeTrailers(['x' => ['y']]);
            } catch (HeadNotWrittenError $e) {
                \Rapira\log('trailers-no-head', context: ['class' => $e::class]);
                $ex->writeBody('caught');
            }
            continue;
        }
        if ($probe === 'trailers-forbidden') {
            $ex->writeHead(200);
            try {
                $ex->writeTrailers(['content-length' => ['5']]);
            } catch (ValueError) {
                $ex->writeBody('rejected');
            }
            continue;
        }
        if ($probe === 'declared-cl') {
            $ex->writeHead(200, ['content-length' => ['10']]);
            // under-run: legal here, the plugin closes the connection
            $ex->writeBody('abc');
            continue;
        }
        $ex->writeBody('unknown probe');
    }
} catch (ClosedException) {
}
