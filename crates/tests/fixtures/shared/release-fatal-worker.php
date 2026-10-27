<?php
// The destructor of a pending exception fatals when the host releases the exception after the unit.

class FatalOnRelease extends \RuntimeException
{
    public function __destruct()
    {
        trigger_error('fatal while the exception is released', E_USER_ERROR); // E_USER_ERROR -> zend_bailout
    }
}

class Counter
{
    public static int $n = 0;
}

$handler = static function (): void {
    Counter::$n++;
    $boom = $_GET['boom'] ?? '';
    if ($boom === 'shutdown') {
        // the shutdown functions of a unit run with no user frame, so nothing reports the exception and it stays pending
        register_shutdown_function(static function (): void {
            throw new FatalOnRelease('from a shutdown function');
        });
    }
    if ($boom === 'teardown') {
        // the teardown flush runs the callback; PHP passes the buffer through, and the exception stays pending
        ob_start(static function (string $buf): string {
            throw new FatalOnRelease('from an output callback');
        });
        echo 'buffered';
        return;
    }
    echo 'ok counter=' . Counter::$n . ' pid=' . getmypid();
};

while (\Rapira\handle_request($handler)) {
}
