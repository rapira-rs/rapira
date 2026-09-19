<?php

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;

$dispatcher = \Rapira\get_dispatcher();

try {
    while (true) {
        $call = $dispatcher->receive();

        try {
            $metadata = $call->getResponseMetadata();
            $metadata->addHeader('x-worker', (string) getmypid());
            $metadata->addTrailer('x-result', 'echoed');
            $call->respond($call->getMessage());
        } catch (WorkDiscardedException) {
        }
    }
} catch (ClosedException) {
}
