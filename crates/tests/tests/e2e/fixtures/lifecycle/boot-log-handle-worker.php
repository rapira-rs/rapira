<?php
// The worker-mode version of boot-log-worker.php. It logs each boot and the end of each app instance.

\Rapira\log('booted');
while (\Rapira\handle_request(static function (): void {
    echo getmypid();
})) {
}
\Rapira\log('app loop ended');
