<?php
$handler = static function (): void {
	// a space is not a tchar, and 0x01 is not a legal field-value byte. sapi_header_op checks only CR, LF and NUL, so both reach the SAPI, which must drop them
	header('Content Type: text/html');
	header("X-Ctl: \x01");
	header('X-Keep: kept');
	http_response_code(201);
	echo 'body';
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
