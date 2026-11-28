<?php

require __DIR__ . '/hold.php';

$app = bin2hex(random_bytes(16));
$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $target = $exchange->getRequest()->target;
        if (str_starts_with($target, '/hold')) {
            $last = null;
            hold_until_release(static function () use ($dispatcher, $target, &$last): void {
                $info = $dispatcher->getInfo();
                $pending = $info->pendingCount();
                if ($last !== $pending) {
                    \Rapira\log("$target pending $pending", context: ['active' => $info->activeCount()]);
                    $last = $pending;
                }
            });
        }
        $exchange->writeBody(json_encode(['target' => $target, 'pid' => getmypid(), 'app' => $app], JSON_THROW_ON_ERROR));
    }
} catch (\Rapira\Exception\ClosedException) {
}
