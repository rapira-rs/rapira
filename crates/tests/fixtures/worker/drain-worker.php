<?php
// handle_request() returns false when the SAPI closes the intake. The loop then exits, and the entrypoint still runs to its end.
$served = 0;
$handler = static function () use (&$served): void {
    $served++;
    echo 'n=', $served;
};
while (\Rapira\handle_request($handler)) {
}
\Rapira\log('loop-exited served=' . $served);
