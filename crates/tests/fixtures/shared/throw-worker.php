<?php
$handler = static function (): void {
	if (($_GET['boom'] ?? '') === '1') {
		throw new \RuntimeException("handler error");
	}
	echo "ok";
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
