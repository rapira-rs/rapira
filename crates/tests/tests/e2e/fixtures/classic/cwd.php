<?php
// Classic mode runs this file once per request and prints getcwd(). /chdir moves the request to the temp directory first.
header('Content-Type: text/plain');

if (($_SERVER['REQUEST_URI'] ?? '') === '/chdir') {
    chdir(sys_get_temp_dir());
}

echo getcwd();
