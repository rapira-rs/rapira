<?php
$handler = static function (): void {
	if (!extension_loaded('sqlite3')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		// the constructor throws when the open fails, and query() with bad SQL only warns
		new SQLite3('/nonexistent-dir-xyz/db.sqlite');
		return;
	}
	echo 'sqlite:' . (new SQLite3(':memory:'))->querySingle('SELECT 40+2');
};
while (\Rapira\handle_request($handler)) {
}
