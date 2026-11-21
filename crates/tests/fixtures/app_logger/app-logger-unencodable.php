<?php

// Values that json_encode cannot represent. They must not throw or lose the record, so PHP runs on to the echo.
$fh = fopen('php://memory', 'rb');

\Rapira\log('hostile', \Rapira\LogLevel::Error, [
    'keep' => 'visible',
    'closure' => static fn (): int => 1,
    'resource' => $fh,
    'nan' => NAN,
    'inf' => INF,
    // Raw bytes from the wire. Invalid UTF-8 is the usual json_encode failure.
    'bytes' => "\xC3\x28\xA0\xA1",
    // A pure enum is not JsonSerializable and has no backing value.
    'pure_enum' => \Rapira\LogLevel::Debug,
]);

fclose($fh);
echo 'logged';
