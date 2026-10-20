<?php
$handler = static function (): void {
	$input = file_get_contents('php://input');
	echo count($_POST), '|', end($_POST) ?: '-', '|', strlen($input), '|', $input;
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
