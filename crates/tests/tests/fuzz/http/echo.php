<?php

// Echoes the Request as JSON for the HTTP fuzz target. Every byte string is base64, so the check compares bytes, not text.

// Request::$headers and the part headers as a list of [name, values]. PHP makes a decimal name an int key, so the cast restores the string.
$headers = static function (array $h): array {
    $out = [];
    foreach ($h as $name => $values) {
        $out[] = [base64_encode((string) $name), array_map('base64_encode', $values)];
    }
    return $out;
};

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        $out = [
            'method' => base64_encode($req->method),
            'uri' => base64_encode($req->uri),
            'target' => base64_encode($req->target),
            'headers' => $headers($req->headers),
        ];
        if ($req->body instanceof \Rapira\Http\Multipart) {
            $out['kind'] = 'multipart';
            $out['fields'] = array_map(static fn(\Rapira\Http\FormField $f): array => [
                'name' => base64_encode($f->name),
                'value' => base64_encode($f->value),
                'headers' => $headers($f->headers),
            ], $req->body->fields);
            $out['files'] = array_map(static fn(\Rapira\Http\UploadedFile $u): array => [
                'name' => base64_encode($u->name),
                'filename' => base64_encode($u->clientFilename),
                'type' => $u->clientMediaType === null ? null : base64_encode($u->clientMediaType),
                'size' => $u->size,
                'content' => base64_encode((string) file_get_contents($u->tmpPath)),
                'headers' => $headers($u->headers),
            ], $req->body->files);
        } else {
            $out['kind'] = 'raw';
            $out['body'] = base64_encode($req->body);
        }
        $ex->writeHead(200, ['content-type' => ['application/json']]);
        $ex->writeBody(json_encode($out, JSON_THROW_ON_ERROR));
    }
} catch (\Rapira\Exception\ClosedException) {
}
