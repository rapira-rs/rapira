<?php

$app = bin2hex(random_bytes(8));
$handler = static function () use ($app): void {
    if (isset($_GET['sleep'])) {
        // On macOS, PHP checks the interrupt flags after each one-second sleep. On other platforms, the wake signal interrupts sleep().
        $interval = PHP_OS_FAMILY === 'Darwin' ? 1 : 10;
        for ($elapsed = 0; $elapsed < 10; $elapsed += $interval) {
            sleep($interval);
        }
    } elseif (isset($_GET['read'])) {
        $socket = stream_socket_client('unix://' . dirname(__DIR__) . '/peer.sock');
        stream_set_timeout($socket, 20);
        fread($socket, 1);
        fclose($socket);
    }
    $body = isset($_GET['app']) ? $app : 'ok';
    header('Content-Length: ' . strlen($body));
    echo $body;
};

while (\Rapira\handle_request($handler)) {
}
