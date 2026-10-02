<?php

// Echoes the Request as JSON for the HTTP fuzz target. Every byte string is base64, so the check compares bytes, not text.

$b64 = static fn(string $s): string => base64_encode($s);

// Request::$headers and the part headers as a list of [name, values]. PHP makes a decimal name an int key, so the cast restores the string.
$headers = static function (array $h) use ($b64): array {
    $out = [];
    foreach ($h as $name => $values) {
        $out[] = [$b64((string) $name), array_map($b64, $values)];
    }
    return $out;
};

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        $out = [
            'method' => $b64($req->method),
            'uri' => $b64($req->uri),
            'target' => $b64($req->target),
            'authority' => $req->authority === null ? null : $b64($req->authority),
            'protocol' => $req->protocol,
            'headers' => $headers($req->headers),
        ];
        if ($req->body instanceof \Rapira\Http\Multipart) {
            $out['kind'] = 'multipart';
            $out['fields'] = array_map(static fn(\Rapira\Http\FormField $f): array => [
                'name' => $b64($f->name),
                'value' => $b64($f->value),
                'headers' => $headers($f->headers),
            ], $req->body->fields);
            $out['files'] = array_map(static fn(\Rapira\Http\UploadedFile $u): array => [
                'name' => $b64($u->name),
                'filename' => $b64($u->clientFilename),
                'type' => $u->clientMediaType === null ? null : $b64($u->clientMediaType),
                'size' => $u->size,
                'content' => $b64((string) file_get_contents($u->tmpPath)),
                'headers' => $headers($u->headers),
            ], $req->body->files);
        } else {
            $out['kind'] = 'raw';
            $out['body'] = $b64($req->body);
        }
        $ex->writeHead(200, ['content-type' => ['application/json']]);
        $ex->writeBody(json_encode($out, JSON_THROW_ON_ERROR));
    }
} catch (\Rapira\Exception\ClosedException) {
}
