<?php
// A shutdown function registered during a unit runs once, at the end of that unit. The boot-registered one runs at cycle end.
$bootFired = 0;
$unitFired = 0;
register_shutdown_function(static function () use (&$bootFired): void {
    $bootFired++;
    \Rapira\log('unit-fixture boot fired=' . $bootFired);
});

$req = 0;
$handler = static function () use (&$unitFired, &$req, &$bootFired): void {
    $req++;
    if ($req === 1) {
        register_shutdown_function(static function () use (&$unitFired): void {
            $unitFired++;
            \Rapira\log('unit-fixture unit fired=' . $unitFired);
        });
    }
    echo 'req=', $req, ' unit_fired=', $unitFired, ' boot_fired=', $bootFired;
};
while (\Rapira\handle_request($handler)) {
}
