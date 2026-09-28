<?php
return json_encode([
    'BOOT_PROBE' => $_SERVER['BOOT_PROBE'] ?? null,
    'PHP_SELF' => $_SERVER['PHP_SELF'] ?? null,
    'SCRIPT_NAME' => $_SERVER['SCRIPT_NAME'] ?? null,
    'SCRIPT_FILENAME' => $_SERVER['SCRIPT_FILENAME'] ?? null,
    'PATH_TRANSLATED' => $_SERVER['PATH_TRANSLATED'] ?? null,
    'DOCUMENT_ROOT' => $_SERVER['DOCUMENT_ROOT'] ?? null,
    'argv' => $_SERVER['argv'] ?? null,
    'argc' => $_SERVER['argc'] ?? null,
]);
