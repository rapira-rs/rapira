<?php
class ThrowingWriteHandler extends \SessionHandler
{
    public function write(string $id, string $data): bool
    {
        throw new \RuntimeException('session write failed');
    }
}
$mode = getenv('APP_EXCEPTION_HANDLER');
if ($mode !== 'none') {
    set_exception_handler(static function (\Throwable $e) use ($mode): void {
        \Rapira\log('handled', \Rapira\LogLevel::Error, ['message' => $e->getMessage()]);
        if ($mode === 'session_start') {
            session_start(); // the teardown runs the handler after the session write of the unit
        }
    });
}
session_set_save_handler(new ThrowingWriteHandler(), false);
$handler = static function (): void {
    $status = session_status(); // PHP_SESSION_NONE (1) at the start of each unit
    session_start();
    $_SESSION['n'] = 1; // no session_write_close(): the teardown writes the session
    echo 'session_status=', $status;
};
while (\Rapira\handle_request($handler)) {
    gc_collect_cycles();
}
