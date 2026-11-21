<?php
// http_response_code(103) stays in effect until the final head. The plugin must change it, because hyper rewrites a final 1xx from the service to a 500 and ends the connection with an error.
$handler = static function (): void {
	http_response_code(103);
	echo "body";
};
while (\Rapira\handle_request($handler)) {
}
