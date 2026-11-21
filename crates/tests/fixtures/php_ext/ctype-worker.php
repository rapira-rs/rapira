<?php
$handler = static function (): void {
	if (!extension_loaded('ctype')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		// ctype_* take mixed, so only a missing argument throws
		ctype_digit();
		return;
	}
	echo 'ctype:' . (ctype_digit('12345') ? '1' : '0');
};
while (\Rapira\handle_request($handler)) {
}
