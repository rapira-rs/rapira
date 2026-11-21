<?php
// exit() inside a handler finishes that response and keeps the resident loop and its state alive. It is not a recycle.
$n = 0;
$handler = static function () use (&$n): void {
    $n++;
    echo 'n=', $n;
    if (($_GET['die'] ?? '') === '1') {
        exit;
    }
};
while (\Rapira\handle_request($handler)) {
}
