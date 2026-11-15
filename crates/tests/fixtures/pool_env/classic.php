<?php
header('Content-Type: application/json');
echo json_encode([
    'getenv' => getenv('POOL_PROBE'),
    'getenv_empty' => getenv('POOL_EMPTY'),
    'getenv_software' => getenv('SERVER_SOFTWARE'),
    'server' => $_SERVER['POOL_PROBE'] ?? null,
    'server_empty' => $_SERVER['POOL_EMPTY'] ?? null,
    'server_inherited' => $_SERVER['POOL_INHERITED'] ?? null,
    'server_software' => $_SERVER['SERVER_SOFTWARE'] ?? null,
]);
$_SERVER['POOL_PROBE'] .= '-changed';
