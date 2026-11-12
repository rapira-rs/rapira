<?php
// Each release folder of the deploy test gets a copy. The code value comes through the current symlink, so the PHP realpath cache must forget the old target.
header('Content-Type: text/plain');
echo 'code=', require dirname(__DIR__, 2) . '/current/release.php', ' cwd=', getcwd(), ' file=', file_get_contents('release.txt');
