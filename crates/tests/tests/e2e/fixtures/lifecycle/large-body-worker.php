<?php
// probe=large writes 10 MiB as 160 writes of one 64 KiB string. probe=fatal writes the same pieces with eos false and then calls an undefined function. probe=stall writes 12 MiB as 12 writes of one 1 MiB string, the last with eos true: the 14 frames fit in the frame channel, so the writes never wait. Any other target gets the instance id.
// The pieces repeat the instance id, so a piece that a later app instance overwrote shows in the body.

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;

$instance = bin2hex(random_bytes(8));
\Rapira\log('booted', context: ['instance' => $instance]);
$piece = str_repeat($instance, 4096);
$mib = str_repeat($instance, 65536);
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        $probe = $q['probe'] ?? '';
        if ($probe === 'stall') {
            for ($i = 0; $i < 12; $i++) {
                $ex->writeBody($mib, eos: $i === 11);
            }
            continue;
        }
        if ($probe !== 'large' && $probe !== 'fatal') {
            $ex->writeBody($instance);
            continue;
        }
        try {
            for ($i = 0; $i < 160; $i++) {
                $ex->writeBody($piece, eos: false);
            }
            if ($probe === 'fatal') {
                rapira_e2e_undefined_function();
            }
            $ex->writeBody('', eos: true);
        } catch (WorkDiscardedException) {
            // the write timeout closed the connection
        }
    }
} catch (ClosedException) {
}
