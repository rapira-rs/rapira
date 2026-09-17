<?php

\Rapira\log('trace-idle');
while (\Rapira\handle_request(static function (): void {
    \Rapira\log('trace-active');
    register_shutdown_function(static function (): void {
        \Rapira\log('trace-shutdown');
        if (isset($_GET['shutdown-fatal'])) {
            trigger_error('trace-shutdown-fatal', E_USER_ERROR);
        }
    });
    echo 'ok';
    if (isset($_GET['throw'])) {
        throw new \RuntimeException('trace-throw');
    }
    if (isset($_GET['fatal'])) {
        trigger_error('trace-fatal', E_USER_ERROR);
    }
    if (isset($_GET['exit'])) {
        exit;
    }
    if (isset($_GET['finish'])) {
        rapira_finish_request();
        \Rapira\log('trace-after-response');
    }
})) {
    \Rapira\log('trace-idle');
}
