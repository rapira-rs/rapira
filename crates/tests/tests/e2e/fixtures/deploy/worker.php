<?php
// Each release folder of the deploy test gets a copy; release.php and release.txt name the release. The "booted" record tells the test that an app boot has required release.php.
$release = require __DIR__ . '/release.php';
\Rapira\log('booted');
$handler = static function () use ($release): void {
    header('Content-Type: text/plain');
    echo "code=$release cwd=", getcwd(), ' file=', file_get_contents('release.txt');
};
while (\Rapira\handle_request($handler)) {
}
