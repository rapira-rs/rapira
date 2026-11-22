<?php

function pool_view(?\Rapira\Dispatcher $boot = null): string
{
    try {
        $current = \Rapira\get_dispatcher();
        $dispatcher = ['name' => $current->name(), 'same' => $current === $boot];
    } catch (\Rapira\Exception\NoDispatcherError $e) {
        $dispatcher = $e::class;
    }
    return json_encode([
        'pid' => getmypid(),
        'mode' => \Rapira\get_mode()->name,
        'dispatcher' => $dispatcher,
        'cwd' => getcwd(),
        'pool' => getenv('RAPIRA_POOL_VIEW'),
        'http' => getenv('RAPIRA_HTTP_ONLY'),
        'grpc' => getenv('RAPIRA_GRPC_ONLY'),
    ], JSON_THROW_ON_ERROR);
}
