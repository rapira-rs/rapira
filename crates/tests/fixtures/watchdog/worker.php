<?php

$handler = static function (): void {
    if (isset($_GET['sleep'])) {
        // On macOS, PHP checks the interrupt flags after each one-second sleep. On other platforms, the wake signal interrupts sleep().
        $interval = PHP_OS_FAMILY === 'Darwin' ? 1 : 10;
        for ($elapsed = 0; $elapsed < 10; $elapsed += $interval) {
            sleep($interval);
        }
    } elseif (isset($_GET['derive'])) {
        // Derive a fixed block of test keys in one native call.
        hash_pbkdf2('sha3-512', 'test password', 'watchdog fixture', 1_000_000, 1024, true);
    }
    header('Content-Length: 2');
    echo 'ok';
};

while (\Rapira\handle_request($handler)) {
}
