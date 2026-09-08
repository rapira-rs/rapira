<?php
$export = null;
$requests = 0;
$handler = static function () use (&$export, &$requests): void {
    ++$requests;
    header('Content-Type: text/plain');
    if ($_SERVER['REQUEST_METHOD'] === 'POST') {
        $export = new SplTempFileObject();
        $export->fwrite(file_get_contents('php://input'));
        echo getmypid(), ':', $requests, ':stored';
        return;
    }
    $export->rewind();
    echo getmypid(), ':', $requests, ':', $export->fgets();
};
while (\Rapira\handle_request($handler)) {
}
