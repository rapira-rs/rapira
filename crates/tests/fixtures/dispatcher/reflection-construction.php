<?php

$cases = [
    ['name' => 'http_dispatcher', 'class' => Rapira\Internal\Http\Dispatcher::class],
    ['name' => 'http_info', 'class' => Rapira\Internal\Http\DispatcherInfo::class],
    ['name' => 'http_exchange', 'class' => Rapira\Internal\Http\Exchange::class],
    ['name' => 'grpc_dispatcher', 'class' => Rapira\Internal\Grpc\Dispatcher::class],
    ['name' => 'grpc_info', 'class' => Rapira\Internal\Grpc\DispatcherInfo::class],
    ['name' => 'grpc_call', 'class' => Rapira\Internal\Grpc\UnaryCall::class],
    ['name' => 'grpc_metadata', 'class' => Rapira\Internal\Grpc\ResponseMetadata::class],
];
$result = [];
foreach ($cases as $case) {
    $class = new ReflectionClass($case['class']);
    try {
        $class->newInstanceWithoutConstructor();
        $result[$case['name']] = 'constructed';
    } catch (Throwable $error) {
        $result[$case['name']] = get_class($error);
    }
}

$dispatcher = Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $exchange->writeBody(json_encode($result, JSON_THROW_ON_ERROR));
    }
} catch (Rapira\Exception\ClosedException) {
}
