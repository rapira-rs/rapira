<?php
$handler = static function (): void {
	if (!extension_loaded('iconv')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		// an unknown encoding only warns, and a non-string argument throws
		iconv('UTF-8', 'UTF-8', []);
		return;
	}
	echo 'iconv:' . iconv('UTF-8', 'UTF-8', 'iconv ok');
};
while (\Rapira\handle_request($handler)) {
}
