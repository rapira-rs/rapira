<?php
// Each release folder of the deploy test gets a copy; release.php and release.txt name the release.
header('Content-Type: text/plain');
echo 'code=', require __DIR__ . '/release.php', ' cwd=', getcwd(), ' file=', file_get_contents('release.txt');
