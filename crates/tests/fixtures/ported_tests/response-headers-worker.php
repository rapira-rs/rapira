<?php
$handler = static function (): void {
	header('Foo: bar');
	header('Foo2: bar2');
	header('Invalid');
	header('I: ' . ($_GET['i'] ?? ''));
	header('Php-Version: ' . PHP_VERSION);
	http_response_code(200 + (int)($_GET['i'] ?? 0));
	echo implode("\n", headers_list());
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
