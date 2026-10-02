<?php

// Replies to each call of the Connect JSON fuzz target with its request message.

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $call = $d->receive();
        $call->respond($call->getMessage());
    }
} catch (\Rapira\Exception\ClosedException) {
}
