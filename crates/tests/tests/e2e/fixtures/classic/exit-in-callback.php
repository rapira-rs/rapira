<?php
// PHP calls the exception handler and the shutdown functions with no frame on the stack, so exit() in them ends the script with a bailout.

switch ($_GET['exit'] ?? '') {
    case 'handler':
        set_exception_handler(static function (Throwable $e): void {
            http_response_code(404);
            echo "Not Found\n";
            exit(1);
        });
        throw new RuntimeException('no route');
    case 'shutdown':
        echo "ok\n";
        register_shutdown_function(static function (): void {
            echo "bye\n";
            exit(1);
        });
        break;
}
