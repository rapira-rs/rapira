<?php
$_ENV['boot_mark'] = 'set-at-boot';
$handler = static function (): void {
    // a newly compiled file that mentions $_ENV triggers the wipe
    require __DIR__ . '/late-env.php';
    echo $_ENV['boot_mark'] ?? 'lost';
};
while (\Rapira\handle_request($handler)) {
}
