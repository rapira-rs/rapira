<?php
// get_browser() with a null name reads $_SERVER['HTTP_USER_AGENT'], so this fixture needs worker mode.
$app = bin2hex(random_bytes(8));
$handler = static function () use ($app): void {
	$probe = ($_GET['probe'] ?? '') === 'app';
	$b = get_browser($probe ? 'Rapira/1.0 (Darwin)' : null, true);
	if (!is_array($b)) {
		echo 'browscap:false';
		return;
	}
	$keys = ['browser', 'platform', 'crawler', 'version', 'parent', 'browser_name_pattern', 'browser_name_regex'];
	$out = [];
	foreach ($keys as $k) {
		$out[] = $k . '=' . (array_key_exists($k, $b) ? $b[$k] : '<unset>');
	}
	echo implode("\n", $out);
	if ($probe) {
		echo "\napp=" . $app;
	}
};
while (\Rapira\handle_request($handler)) {
}
