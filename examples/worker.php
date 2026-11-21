<?php
// The worker runs this file at start and again after each recycle. The handler runs for every request.
$booted = date(DATE_ATOM);

$handler = static function () use ($booted): void {
    header('content-type: text/plain');
    echo 'worker: ', $_SERVER['REQUEST_METHOD'], ' ', $_SERVER['REQUEST_URI'], "\n";
    echo 'booted: ', $booted, "\n";
};

while (\Rapira\handle_request($handler)) {
}
