<?php

$outside = \Rapira\trace_context();
$previous = [];
while (\Rapira\handle_request(static function () use (&$outside, &$previous): void {
    $active = \Rapira\trace_context();
    echo json_encode(['outside' => $outside, 'active' => $active, 'previous' => $previous]);
    $previous = $active;
    register_shutdown_function(static function () use ($active): void {
        \Rapira\log('trace-shutdown', context: ['same' => \Rapira\trace_context() === $active]);
    });
    if (isset($_GET['exit'])) {
        exit;
    }
})) {
    $outside = \Rapira\trace_context();
}
\Rapira\log('trace-outside', context: ['carrier' => \Rapira\trace_context()]);
