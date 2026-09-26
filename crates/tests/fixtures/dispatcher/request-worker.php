<?php

// Echoes every Request field into the response body for the field-mapping tests.

$d = \Rapira\get_dispatcher();
// the error message of a userland write to an initialized readonly property
$denied = static function (object $o, string $prop): string {
    try {
        $o->$prop = 'x';
        return 'written';
    } catch (\Error $e) {
        return $e->getMessage();
    }
};
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        // the memo: repeated calls hand back the same instance
        $again = $ex->getRequest();
        $lines = [
            'method=' . $req->method,
            'uri=' . $req->uri,
            'target-hex=' . bin2hex($req->target),
            'authority=' . var_export($req->authority, true),
            'protocol=' . $req->protocol,
            // repeats stay separate list entries, wire order
            'x-probe=' . implode('|', $req->headers['x-probe'] ?? []),
            // an all-digit field name must land as an integer key (symtable)
            'h123=' . implode(',', $req->headers[123] ?? []),
            // a single-letter name must stay a string key
            'h-single=' . implode(',', $req->headers['a'] ?? []),
            // '-'-leading names pin the symtable prefilter's one-byte overread
            'h-dash=' . implode(',', $req->headers['-'] ?? []),
            'h-neg=' . implode(',', $req->headers['-1'] ?? []),
            'memo-same=' . var_export($req === $again, true),
            'body=' . (is_string($req->body) ? $req->body : $req->body::class),
            'remote=' . $req->remote::class,
            'remote-detail=' . ($req->remote instanceof \Rapira\InetAddress
                ? $req->remote->ip . ':' . $req->remote->port
                : var_export($req->remote->path, true)),
            'server=' . $req->server::class,
            'server-detail=' . ($req->server instanceof \Rapira\InetAddress
                ? $req->server->ip . ':' . $req->server->port
                : var_export($req->server->path, true)),
            'tls=' . ($req->tls === null ? 'NULL' : implode('|', [
                $req->tls->version,
                $req->tls->cipher,
                var_export($req->tls->negotiatedProtocol, true),
                var_export($req->tls->requestedServerName, true),
                var_export($req->tls->certSerial, true),
                var_export($req->tls->certOrganization, true),
                var_export($req->tls->certFingerprint, true),
            ])),
            'received-at=' . var_export($req->receivedAt, true),
            'received-at-positive=' . var_export($req->receivedAt > 0, true),
            'readonly-request=' . $denied($req, 'method'),
            'readonly-remote=' . $denied($req->remote, $req->remote instanceof \Rapira\InetAddress ? 'ip' : 'path'),
        ];
        $ex->writeBody(implode("\n", $lines));
    }
} catch (\Rapira\Exception\ClosedException) {
}
