<?php
// Each release folder of the deploy test gets a copy; release.php and release.txt name the release.
$release = require __DIR__ . '/release.php';
$handler = static function () use ($release): void {
    header('Content-Type: text/plain');
    echo "code=$release cwd=", getcwd(), ' file=', file_get_contents('release.txt');
};
while (\Rapira\handle_request($handler)) {
}
