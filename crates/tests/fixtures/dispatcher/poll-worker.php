<?php

// Serves through tryReceive(), then through receive() with a 1 s timeout after a request changes the mode.

$d = \Rapira\get_dispatcher();
$mode = 'try';
while (true) {
    try {
        if ($mode === 'try') {
            $ex = $d->tryReceive();
            if ($ex === null) {
                usleep(1000);
                continue;
            }
        } else {
            $ex = $d->receive(1_000_000);
        }
    } catch (\Rapira\Exception\TimeoutException) {
        continue;
    } catch (\Rapira\Exception\ClosedException) {
        break;
    }
    $req = $ex->getRequest();
    parse_str(parse_url($req->target, PHP_URL_QUERY) ?: '', $q);
    $served = $mode;
    if (($q['mode'] ?? '') !== '') {
        $mode = $q['mode'];
    }
    $ex->writeBody("served-by=$served target={$req->target}");
}
