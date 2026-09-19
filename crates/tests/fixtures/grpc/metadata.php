<?php

$dispatcher = Rapira\get_dispatcher();
try {
    while ($work = $dispatcher->receive()) {
        try {
            $text = 'first';
            $values = [&$text, 'second'];
            $metadata = new Rapira\Grpc\Metadata(['x-text' => &$values, 'x-data-bin' => ["\x00\xff"]]);
            $text = 'changed';
            $values[] = 'third';
            $result = [
                'snapshot' => $metadata->values('X-Text'),
                'binary' => bin2hex($metadata->values('x-data-bin')[0]),
                'count' => count($metadata),
                'iteration' => array_keys(iterator_to_array($metadata)),
                'missing' => $metadata->values('absent'),
            ];
            $cases = [
                ['name' => 'empty_name', 'entries' => ['' => ['x']]],
                ['name' => 'uppercase', 'entries' => ['X-Key' => ['x']]],
                ['name' => 'invalid_name', 'entries' => ['x:key' => ['x']]],
                ['name' => 'non_ascii_name', 'entries' => ["x-\xff" => ['x']]],
                ['name' => 'text_control', 'entries' => ['x-key' => ["\t"]]],
                ['name' => 'text_non_ascii', 'entries' => ['x-key' => ["\xff"]]],
                ['name' => 'not_a_list', 'entries' => ['x-key' => [1 => 'x']]],
                ['name' => 'not_a_string', 'entries' => ['x-key' => [1]]],
                ['name' => 'scalar_values', 'entries' => ['x-key' => 'x']],
            ];
            foreach ($cases as $case) {
                try {
                    new Rapira\Grpc\Metadata($case['entries']);
                    $result[$case['name']] = 'accepted';
                } catch (Throwable $error) {
                    $result[$case['name']] = get_class($error);
                }
            }
            $uninitialized = (new ReflectionClass(Rapira\Grpc\Metadata::class))->newInstanceWithoutConstructor();
            $cases = [
                ['name' => 'uninitialized_count', 'run' => fn() => count($uninitialized)],
                ['name' => 'uninitialized_values', 'run' => fn() => $uninitialized->values('x')],
                ['name' => 'uninitialized_iterator', 'run' => fn() => $uninitialized->getIterator()],
            ];
            foreach ($cases as $case) {
                try {
                    $case['run']();
                    $result[$case['name']] = 'accepted';
                } catch (Throwable $error) {
                    $result[$case['name']] = get_class($error);
                }
            }
            $work->writeBody(json_encode($result, JSON_THROW_ON_ERROR));
        } catch (Throwable $error) {
            $work->writeBody(json_encode(['error' => $error->getMessage()]));
        }
    }
} catch (Rapira\Exception\ClosedException) {
}
