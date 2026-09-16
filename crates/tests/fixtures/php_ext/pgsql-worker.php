<?php
$handler = static function (): void {
	if (!extension_loaded('pgsql')) {
		echo 'skip';
		return;
	}
	echo 'pgsql:' . bin2hex(pg_unescape_bytea('\\x0001ff'));
};
while (\Rapira\handle_request($handler)) {
}
