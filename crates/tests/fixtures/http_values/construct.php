<?php

use Rapira\Http\FormField;
use Rapira\Http\Multipart;
use Rapira\Http\Request;
use Rapira\Http\UploadedFile;
use Rapira\InetAddress;
use Rapira\Tls;
use Rapira\UnixAddress;

$remote = new InetAddress('203.0.113.7', 44123);
$server = new UnixAddress(null);
$tls = new Tls('TLSv1.3', 'TLS_AES_128_GCM_SHA256', 'h2', 'example.test', null, null, null);
$field = new FormField('note', 'hello', ['content-type' => ['text/plain']]);
$file = new UploadedFile('avatar', 'me.png', 'image/png', [], '/tmp/spool-1', 512);
$body = new Multipart([$field], [$file]);

$req = new Request(
    'POST',
    '/upload?x=1',
    '/upload?x=1',
    'example.test:8443',
    'HTTP/2',
    ['host' => ['example.test:8443']],
    $body,
    $remote,
    $server,
    $tls,
    1722700000.25,
);

echo $req->method, ' ', $req->target, ' ', $req->protocol, "\n";
echo $req->remote->ip, ':', $req->remote->port, "\n";
echo 'server-path=', var_export($req->server->path, true), "\n";
echo $req->body->fields[0]->name, '=', $req->body->fields[0]->value, "\n";
echo $req->body->files[0]->clientFilename, ' ', $req->body->files[0]->size, "\n";
echo $req->tls->negotiatedProtocol, ' ', var_export($req->tls->certSerial, true), "\n";
echo $req->authority, ' ', $req->receivedAt, "\n";

// tls: null is a legal Request (plain-HTTP listener) - must construct, not crash.
$plain = new Request('GET', '/', '/', null, 'HTTP/1.1', [], '', $remote, $server, null, 1.0);
echo 'tls-null: ', var_export($plain->tls, true), "\n";

try {
    $req->method = 'GET';
    echo "reassigned\n";
} catch (\Error) {
    echo "readonly: enforced\n";
}

// the arity rejection is what proves the constructor exists at all
try {
    new Request('GET');
    echo "partial accepted\n";
} catch (\ArgumentCountError) {
    echo "arity: enforced\n";
}

try {
    new Request('GET', '/', '/', null, 'HTTP/1.1', [], '', 'nope', $server, null, 0.0);
    echo "union accepted\n";
} catch (\TypeError) {
    echo "union: enforced\n";
}

echo 'done';
