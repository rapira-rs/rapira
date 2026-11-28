<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    if (($_GET['mode'] ?? '') === 'spin') {
        // The VM checks for a timeout each time the loop repeats.
        while (true) {
        }
    }
    echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
