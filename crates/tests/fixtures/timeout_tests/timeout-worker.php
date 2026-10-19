<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    if (($_GET['mode'] ?? '') === 'spin') {
        // the Zend VM checks vm_interrupt on loop back-edges, so the execution timer's SIGPROF turns this busy loop into a "Maximum execution time exceeded" fatal
        while (true) {
        }
    }
    echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
