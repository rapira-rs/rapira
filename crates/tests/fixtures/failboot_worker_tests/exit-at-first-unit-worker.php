<?php
// Every app instance answers 500 to its first request and ends with exit status 1, as an app that fails to boot at its first request.
\Rapira\handle_request(static function (): void {
    http_response_code(500);
    echo "kernel failed to boot\n";
});
exit(1);
