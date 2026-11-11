<?php
// Each release folder of the deploy test gets a copy. The boot fails when release.php returns 'broken', so OPcache can hold the release.php of a release that cannot boot.
$release = require __DIR__ . '/release.php';
if ($release === 'broken') {
    \Rapira\log('boot failed');
    throw new RuntimeException('the boot of this release fails');
}
\Rapira\log('booted', context: ['code' => $release]);
$handler = static function () use ($release): void {
    header('Content-Type: text/plain');
    if (!extension_loaded('Zend OPcache') || !is_array(opcache_get_status(false))) {
        echo 'skip';
        return;
    }
    echo "code=$release";
};
while (\Rapira\handle_request($handler)) {
}
