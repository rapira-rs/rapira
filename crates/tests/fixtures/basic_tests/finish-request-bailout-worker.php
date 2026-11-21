<?php
// display_errors=Off keeps fatal text out of the stream, so the observable outcome is the 500 head committed by the recycle path
ini_set('display_errors', '0');
class Counter { public static int $n = 0; }
$handler = static function (): void {
    Counter::$n++;
    if (($_GET['boom'] ?? '') === '1') {
        ob_start(static function ($buf, $phase) {
            trigger_error('fatal in flush', E_USER_ERROR); // bails out of php_output_end_all
        });
        echo 'buffered';
        \rapira_finish_request();   // the flush runs the ob handler, which causes the fatal error
        echo 'resumed-after-fatal'; // must never run, because the bailout ends the unit
        return;
    }
    header('Content-Type: text/plain');
    echo 'ok counter=' . Counter::$n;
};
while (\Rapira\handle_request($handler)) {
}