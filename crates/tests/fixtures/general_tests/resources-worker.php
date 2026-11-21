<?php
$handler = static function (): void {
	// the result is not used, and the read creates the stream that the next line counts
	file_get_contents('php://input');
	header('Content-Type: text/plain');
	echo "streams=", count(get_resources('stream'));
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
