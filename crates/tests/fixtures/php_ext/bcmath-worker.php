<?php
$handler = static function (): void {
	if (!extension_loaded('bcmath')) {
		echo 'skip';
		return;
	}
	echo 'bcmath:' . bcadd('0.1', '0.2', 2);
};
while (\Rapira\handle_request($handler)) {
}
