<?php
// Each release folder of the deploy test gets a copy. The app boot requires release.php through the current symlink, so OPcache must forget the script that it holds under that path.
$release = require dirname(__DIR__, 2) . '/current/release.php';
$handler = static function () use ($release): void {
    header('Content-Type: text/plain');
    echo "code=$release cwd=", getcwd(), ' file=', file_get_contents('release.txt');
};
while (\Rapira\handle_request($handler)) {
}
