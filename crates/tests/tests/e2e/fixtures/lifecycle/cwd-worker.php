<?php
// Each unit prints getcwd(). /chdir moves the app to the temp directory first. /child prints PHP_ZTS and the directory of a proc_open() child without a cwd. The "booted" record tells the test that an app instance started.
\Rapira\log('booted');
$handler = static function (): void {
    header('Content-Type: text/plain');
    $uri = $_SERVER['REQUEST_URI'] ?? '';
    if ($uri === '/child') {
        $child = proc_open(['pwd', '-P'], [1 => ['pipe', 'w']], $pipes);
        echo (int) PHP_ZTS, ' ', trim(stream_get_contents($pipes[1]));
        proc_close($child);
        return;
    }
    if ($uri === '/chdir') {
        chdir(sys_get_temp_dir());
    }
    echo getcwd();
};
while (\Rapira\handle_request($handler)) {
}
