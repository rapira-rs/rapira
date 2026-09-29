<?php
// Serves one request and returns; every later boot returns at once: a failed re-boot after the app served.

$flag = __DIR__ . '/served.flag';
if (file_exists($flag)) {
    \Rapira\log('reboot failed');
    return;
}
$ex = \Rapira\get_dispatcher()->receive();
$ex->writeHead(200, ['content-type' => ['text/plain']]);
$ex->writeBody('ok');
touch($flag);
