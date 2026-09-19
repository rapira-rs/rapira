<?php

function cpuMicros(): int {
    $usage = getrusage();
    return ($usage['ru_utime.tv_sec'] + $usage['ru_stime.tv_sec']) * 1000000
        + $usage['ru_utime.tv_usec'] + $usage['ru_stime.tv_usec'];
}

$dispatcher = Rapira\get_dispatcher();
try {
    $work = $dispatcher->receive();
} catch (Rapira\Exception\ClosedException) {
    return;
}
if ($work instanceof Rapira\Grpc\UnaryCall) {
    $action = $work->getMessage();
    $work->respond('ready');
} else {
    $action = ltrim($work->getRequest()->target, '/');
    $work->writeBody('ready');
}
register_shutdown_function(function () {
    Rapira\log('receive-timer-finished');
});
try {
    $result = match ($action) {
        'poll' => $dispatcher->tryReceive(),
        'zero' => $dispatcher->receive(0),
        'finite' => $dispatcher->receive(1000),
        'closed' => $dispatcher->receive(),
    };
    $outcome = $result === null ? 'empty' : 'work';
} catch (Rapira\Exception\TimeoutException) {
    $outcome = 'timeout';
} catch (Rapira\Exception\ClosedException) {
    $outcome = 'closed';
}
Rapira\log('receive-timer-outcome', context: ['outcome' => $outcome]);
$end = cpuMicros() + 2000000;
while (cpuMicros() < $end) {}
Rapira\log('receive-timer-survived');
