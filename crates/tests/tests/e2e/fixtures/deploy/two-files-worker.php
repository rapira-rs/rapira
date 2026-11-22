<?php
// Each release folder of the deploy test gets a copy. The boot fails while extra.php is missing. This cause is outside OPcache.
$release = require __DIR__ . '/release.php';
if (!is_file(__DIR__ . '/extra.php')) {
    \Rapira\log('boot failed');
    throw new RuntimeException('extra.php is missing');
}
$extra = require __DIR__ . '/extra.php';
$app = bin2hex(random_bytes(8));
$handler = static function () use ($app, $release, $extra): void {
    usleep(50_000);
    header('Content-Type: text/plain');
    printf('%s release.php=%s extra.php=%s', $app, $release, $extra);
};
while (\Rapira\handle_request($handler)) {
}
