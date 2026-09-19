<?php

$dispatcher = Rapira\get_dispatcher();
try {
    while ($call = $dispatcher->receive()) {
        if ($call->getContext()->metadata->values('test-case')[0] === 'allocate') {
            $metadata = $call->getResponseMetadata();
            $metadata->addBinaryHeader('x-large-bin', $call->getMessage());
            ini_set('memory_limit', (string)(memory_get_usage(true) + 1048576));
            $metadata->headers();
        }
        $call->respond('next');
    }
} catch (Rapira\Exception\ClosedException) {
}
