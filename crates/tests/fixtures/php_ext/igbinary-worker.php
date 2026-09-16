<?php
$handler = static function (): void {
	if (!extension_loaded('igbinary')) {
		echo 'skip';
		return;
	}
	$value = ['message' => 'rapira', 'count' => 42];
	echo 'igbinary:' . (igbinary_unserialize(igbinary_serialize($value)) === $value ? 'ok' : 'fail');
};
while (\Rapira\handle_request($handler)) {
}
