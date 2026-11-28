<?php

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        parse_str(parse_url($req->target, PHP_URL_QUERY) ?: '', $q);
        $case = $q['case'] ?? 'text';

        if ($case === 'small') {
            $ex->writeHead(200, ['content-type' => ['text/plain']]);
            $ex->writeBody('tiny');
            continue;
        }
        if ($case === 'binary') {
            $ex->writeHead(200, ['content-type' => ['application/octet-stream']]);
            $ex->writeBody(str_repeat('B', 4096));
            continue;
        }
        if ($case === 'preencoded') {
            $ex->writeHead(200, [
                'content-type' => ['text/plain'],
                'content-encoding' => ['br'],
            ]);
            $ex->writeBody(str_repeat('P', 4096));
            continue;
        }
        if ($case === 'notransform') {
            $ex->writeHead(200, [
                'content-type' => ['text/plain'],
                'cache-control' => ['public, no-transform'],
            ]);
            $ex->writeBody(str_repeat('N', 4096));
            continue;
        }
        if ($case === 'stream') {
            $ex->writeHead(200, ['content-type' => ['text/plain']]);
            $ex->writeBody(str_repeat('alpha-', 200), eos: false);
            $ex->writeBody(str_repeat('omega-', 200), eos: true);
            continue;
        }
        if ($case === 'sendfile') {
            $ex->writeHead(200, ['content-type' => ['text/plain']]);
            $ex->sendFile($req->headers['x-path'][0] ?? '');
            continue;
        }
        if ($case === 'head_cl') {
            $ex->writeHead(200, [
                'content-type' => ['text/plain; charset=utf-8'],
                'content-length' => ['2500'],
                'etag' => ['"v1"'],
            ]);
            $ex->writeBody('', eos: true);
            continue;
        }
        $ex->writeHead(200, [
            'content-type' => ['text/plain; charset=utf-8'],
            'etag' => ['"v1"'],
        ]);
        $ex->writeBody(str_repeat('hello rapira compression ', 100));
    }
} catch (ClosedException) {
}
