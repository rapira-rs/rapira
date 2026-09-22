<?php
// Classic mode runs this file once per request; /chdir moves the worker out of the entrypoint directory.
header('Content-Type: text/plain');

if (($_SERVER['REQUEST_URI'] ?? '') === '/chdir') {
    chdir(sys_get_temp_dir());
}

echo getcwd();
