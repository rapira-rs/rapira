<?php
// Resident worker script: handles each exchange a test plugin submits.
$handler = static function (): void {
	header('Content-Type: text/plain');
	echo 'ok:' . ($_GET['from'] ?? '?');
};
while (\Rapira\handle_request($handler)) {
}
