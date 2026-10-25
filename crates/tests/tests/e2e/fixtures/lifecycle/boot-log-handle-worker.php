<?php
// The worker-mode twin of boot-log-worker.php: logs each boot and the end of each app instance.

\Rapira\log('booted');
while (\Rapira\handle_request(static function (): void {
    echo getmypid();
})) {
}
\Rapira\log('app loop ended');
