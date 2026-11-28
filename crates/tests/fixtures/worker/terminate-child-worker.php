<?php
// Each unit starts a sleep child, sends it SIGTERM with proc_terminate() and prints the status that proc_close() returns.
$handler = static function (): void {
    header('Content-Type: text/plain');
    $child = proc_open(['sleep', '5'], [], $pipes);
    proc_terminate($child);
    echo proc_close($child);
};
while (\Rapira\handle_request($handler)) {
}
