<?php
// Each release folder of the deploy test gets a copy. The handler prints the script paths of the $_SERVER of its request.
$handler = static function (): void {
    header('Content-Type: text/plain');
    printf('file=%s root=%s name=%s self=%s', $_SERVER['SCRIPT_FILENAME'], $_SERVER['DOCUMENT_ROOT'], $_SERVER['SCRIPT_NAME'], $_SERVER['PHP_SELF']);
};
while (\Rapira\handle_request($handler)) {
}
