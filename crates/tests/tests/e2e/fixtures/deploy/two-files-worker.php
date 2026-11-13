<?php
// Each release folder of the deploy test gets a copy. The boot fails while extra.php is missing: a cause outside OPcache.
$release = require __DIR__ . '/release.php';
if (!is_file(__DIR__ . '/extra.php')) {
    \Rapira\log('boot failed');
    throw new RuntimeException('extra.php is missing');
}
$extra = require __DIR__ . '/extra.php';
$handler = static function () use ($release, $extra): void {
    header('Content-Type: text/plain');
    printf('pid=%d release.php=%s extra.php=%s', getmypid(), $release, $extra);
};
while (\Rapira\handle_request($handler)) {
}
