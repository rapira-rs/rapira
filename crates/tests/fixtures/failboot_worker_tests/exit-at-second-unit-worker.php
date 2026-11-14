<?php
// Every app instance serves one request, answers 500 to the next one and ends with exit status 1.
\Rapira\handle_request(static function (): void {
    echo "ok\n";
});
\Rapira\handle_request(static function (): void {
    http_response_code(500);
    echo "failed\n";
});
exit(1);
