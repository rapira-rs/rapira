<?php
// null has no refcount: a $_SERVER built from the $argv global after this line crashes the worker
$argv = null;
$boot = require __DIR__ . '/values.php';
$handler = static function () use ($boot): void {
    header('Content-Type: application/json');
    echo $boot;
};
while (\Rapira\handle_request($handler)) {
}
