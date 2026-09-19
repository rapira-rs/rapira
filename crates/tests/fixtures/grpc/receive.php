<?php

$dispatcher = Rapira\get_dispatcher();
$cases = [
    ['name' => 'try', 'run' => fn() => $dispatcher->tryReceive()],
    ['name' => 'zero', 'run' => fn() => $dispatcher->receive(0)],
    ['name' => 'finite', 'run' => fn() => $dispatcher->receive(1000)],
    ['name' => 'negative', 'run' => fn() => $dispatcher->receive(-2)],
];
$results = [];
foreach ($cases as $case) {
    try {
        $results[$case['name']] = $case['run']();
    } catch (Throwable $error) {
        $results[$case['name']] = get_class($error);
    }
}
Rapira\log('grpc-receive', context: $results);
try {
    while ($call = $dispatcher->receive()) {
        $call->respond($call->getMessage());
    }
} catch (Rapira\Exception\ClosedException) {
    Rapira\log('grpc-closed');
}
