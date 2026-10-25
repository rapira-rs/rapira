<?php
// Every boot takes 2 s, and ?slow takes 2 s: a long last request, then a long boot of the next app instance.

use Rapira\Exception\ClosedException;

usleep(2_000_000);
\Rapira\log('booted');
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        if (isset($q['slow'])) {
            usleep(2_000_000);
        }
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
