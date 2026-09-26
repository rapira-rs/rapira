<?php
// draining-false contract: the loop exits when the SAPI closes the intake, and the script still runs to completion.
$served = 0;
$handler = static function () use (&$served): void {
    $served++;
    echo 'n=', $served;
};
while (\Rapira\handle_request($handler)) {
}
\Rapira\log('loop-exited served=' . $served);
