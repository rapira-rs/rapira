<?php
// ?leak=1 keeps small string-keyed arrays until memory_limit ends the app instance with a fatal error.
// The heap then has no free slot of their size class, so the next allocation of that size needs memory past the limit.

ini_set('memory_limit', '16M');
$kept = new SplFixedArray(200000);
$boot = uniqid('', true);
$handler = static function () use ($kept, $boot): void {
    if (isset($_GET['leak'])) {
        for ($i = 0; $i < 200000; $i++) {
            $kept[$i] = ['a' => $i];
        }
    }
    echo 'pid=', getmypid(), ' boot=', $boot;
};

while (\Rapira\handle_request($handler)) {
}
