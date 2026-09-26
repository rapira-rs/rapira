<?php
// the bootstrap opens a session whose save handler fatals on first write, so the worker's first rapira_request_teardown() bails while flushing it; the persistent counter in sys_get_temp_dir() lets the served request report its cycle, which reads "2" once the bailout recycles and re-bootstraps
$dir = sys_get_temp_dir();
$sentinel = $dir . '/rapira_h2_sentinel_' . getmypid();
$boot = $dir . '/rapira_h2_boot_' . getmypid();

$n = (file_exists($boot) ? (int) file_get_contents($boot) : 0) + 1;
file_put_contents($boot, (string) $n);

class BoomOnce extends SessionHandler {
    public string $sentinel = '';
    public function write(string $id, string $data): bool {
        if (!file_exists($this->sentinel)) {
            @touch($this->sentinel);
            trigger_error('boot bail', E_USER_ERROR); // bails inside rapira_reset_session
        }
        return true;
    }
}
$save = new BoomOnce();
$save->sentinel = $sentinel;
session_set_save_handler($save);
session_start();
$_SESSION['k'] = 'v'; // dirty the session so the flush actually writes

$handler = static function () use ($boot): void {
    echo (int) file_get_contents($boot);
};
while (\Rapira\handle_request($handler)) {
}
