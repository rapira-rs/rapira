<?php
header('Content-Type: application/json');
echo json_encode([
    'getenv' => getenv('POOL_PROBE'),
    'getenv_empty' => getenv('POOL_EMPTY'),
    'getenv_software' => getenv('SERVER_SOFTWARE'),
]);
