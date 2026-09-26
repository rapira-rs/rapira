<?php
// http_response_code(103) survives to the final head; the plugin must coerce it, because hyper rewrites a service-supplied final 1xx to a 500 and errors the connection.
$handler = static function (): void {
	http_response_code(103);
	echo "body";
};
while (\Rapira\handle_request($handler)) {
}
