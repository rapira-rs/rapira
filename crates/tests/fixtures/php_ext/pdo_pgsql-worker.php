<?php
$handler = static function (): void {
	if (!extension_loaded('pdo_pgsql')) {
		echo 'skip';
		return;
	}
	echo 'pdo_pgsql:' . (in_array('pgsql', PDO::getAvailableDrivers(), true) ? 'ok' : 'fail');
};
while (\Rapira\handle_request($handler)) {
}
