<?php
class ThrowingWriteHandler extends \SessionHandler
{
    public function write(string $id, string $data): bool
    {
        throw new \RuntimeException('session write failed');
    }
}
if (getenv('APP_EXCEPTION_HANDLER') === 'log') {
    set_exception_handler(static function (\Throwable $e): void {
        \Rapira\log('handled', \Rapira\LogLevel::Error, ['message' => $e->getMessage()]);
    });
}
session_set_save_handler(new ThrowingWriteHandler(), false);
$handler = static function (): void {
    $status = session_status(); // PHP_SESSION_NONE (1) at the start of the unit
    session_start();
    $_SESSION['n'] = 1; // no session_write_close(): the teardown writes the session
    echo 'session_status=', $status;
};
while (\Rapira\handle_request($handler)) {
    gc_collect_cycles();
}
