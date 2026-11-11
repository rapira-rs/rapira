<?php
// Answers with the OPcache state of the worker. ?reset=1 calls opcache_reset().
// ?fill=1 wastes OPcache memory with a changed file and then fills the hash table, so OPcache schedules a restart by itself.
// ?sleep=S holds the request for S seconds.
$fill = static function (): void {
    $dir = __DIR__ . '/gen';
    @mkdir($dir);
    $big = "$dir/big.php";
    for ($v = 0; $v < 2; $v++) {
        file_put_contents($big, "<?php\n\$x = [];\n" . str_repeat("\$x[] = $v * 7;\n", 2000) . "return \$x;\n");
        // a new mtime: OPcache drops the cached copy, and its memory counts as wasted
        touch($big, time() + 10 * $v);
        clearstatcache();
        include $big;
    }
    for ($i = 0; $i < 230; $i++) {
        file_put_contents("$dir/s$i.php", "<?php\nreturn $i;\n");
        include "$dir/s$i.php";
    }
};
$handler = static function () use ($fill): void {
    if (!extension_loaded('Zend OPcache') || !is_array(opcache_get_status(false))) {
        echo 'skip';
        return;
    }
    if (isset($_GET['reset'])) {
        opcache_reset();
    }
    if (isset($_GET['fill'])) {
        $fill();
    }
    if (isset($_GET['sleep'])) {
        sleep((int) $_GET['sleep']);
    }
    $s = opcache_get_status(false);
    printf('pid=%d enabled=%s pending=%s', getmypid(), var_export($s['opcache_enabled'], true), var_export($s['restart_pending'], true));
};
while (\Rapira\handle_request($handler)) {
}
