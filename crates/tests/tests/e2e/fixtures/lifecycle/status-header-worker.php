<?php
// `Status: NNN` is the CGI convention to set the response code, and sapi_header_op treats it as a plain header. So the origin server must consume it, or it goes out as a literal field on a 200.
$handler = static function (): void {
    header('Status: 404 Not Found');
    header('X-Keep: kept');
    echo 'body';
};
while (\Rapira\handle_request($handler)) {
}
