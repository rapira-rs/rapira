<?php

// Each call sleeps for 2 s before its reply. A sleep uses wall-clock time and no CPU time.
$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $call = $dispatcher->receive();
        usleep(2_000_000);
        $call->respond($call->getMessage());
    }
} catch (\Rapira\Exception\ClosedException) {
}
