<?php
$handler = static function (): void {
	if (!extension_loaded('fileinfo')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		// the finfo constructor throws on a missing magic database, and finfo_open() only warns
		new finfo(FILEINFO_NONE, '/nonexistent-dir-xyz/magic.mgc');
		return;
	}
	echo 'finfo:' . (new finfo(FILEINFO_MIME_TYPE))->buffer('hello');
};
while (\Rapira\handle_request($handler)) {
}
