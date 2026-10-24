<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    // the PHP thread waits in poll() and uses no CPU until the test backend answers
    $s = stream_socket_client('tcp://127.0.0.1:' . (int) $_GET['port'], $errno, $error, 5);
    stream_set_timeout($s, 30);
    echo 'read ', trim((string) fgets($s));
};
while (\Rapira\handle_request($handler)) {
}
