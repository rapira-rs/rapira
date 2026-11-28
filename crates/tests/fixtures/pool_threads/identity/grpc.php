<?php

require __DIR__ . '/view.php';

$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $call = $dispatcher->receive();
        $call->getResponseMetadata()->addHeader('x-view', pool_view($dispatcher));
        if ($call->getContext()->metadata->values('x-change') !== []) {
            chdir(dirname(__DIR__));
            putenv('RAPIRA_POOL_VIEW=grpc app');
        }
        $call->respond($call->getMessage());
    }
} catch (\Rapira\Exception\ClosedException) {
}
