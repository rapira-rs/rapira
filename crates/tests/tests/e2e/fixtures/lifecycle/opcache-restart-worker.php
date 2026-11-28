<?php
// Answers with the OPcache state of the worker. ?reset=1 calls opcache_reset().
// ?fill=1 wastes OPcache memory with a changed file and then fills the hash table, so OPcache schedules a restart by itself.
// ?hold=NAME waits for release-NAME. ?probe=1 holds a worker for 50 ms so other probes can reach other workers.
$app = bin2hex(random_bytes(8));
$fill = static function (): void {
    $dir = __DIR__ . '/gen';
    @mkdir($dir);
    $big = "$dir/big.php";
    for ($v = 0; $v < 2; $v++) {
        file_put_contents($big, "<?php\n\$x = [];\n" . str_repeat("\$x[] = $v * 7;\n", 2000) . "return \$x;\n");
        // with a new mtime, OPcache drops the cached copy, and its memory counts as wasted
        touch($big, time() + 10 * $v);
        clearstatcache();
        include $big;
    }
    for ($i = 0; $i < 230; $i++) {
        file_put_contents("$dir/s$i.php", "<?php\nreturn $i;\n");
        include "$dir/s$i.php";
    }
};
$handler = static function () use ($fill, $app): void {
    if (!extension_loaded('Zend OPcache')) {
        echo 'skip';
        return;
    }
    if (isset($_GET['reset'])) {
        opcache_reset();
    }
    if (isset($_GET['fill'])) {
        $fill();
    }
    if (isset($_GET['hold'])) {
        $name = $_GET['hold'];
        \Rapira\log("holding $name", context: ['app' => $app]);
        $until = microtime(true) + 30;
        while (!is_file(__DIR__ . "/release-$name")) {
            if (microtime(true) >= $until) {
                throw new RuntimeException("request $name was not released");
            }
            usleep(1_000);
        }
    } elseif (isset($_GET['probe'])) {
        usleep(50_000);
    }
    $s = opcache_get_status(false);
    printf('%s enabled=%s pending=%s', $app, var_export($s['opcache_enabled'] ?? false, true), var_export($s['restart_pending'] ?? false, true));
};
while (\Rapira\handle_request($handler)) {
}
\Rapira\log("ended $app");
