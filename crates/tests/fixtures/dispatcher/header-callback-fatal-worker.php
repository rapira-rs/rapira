<?php
// The cycle sends no output, so the cycle-end header send runs this callback, and its fatal bails out there.
header_register_callback(static function (): void {
    str_repeat('x', 1 << 40);
});
// These objects take the heap slots of the previous cycle, so a stale dispatcher points at one of them.
$objects = [];
for ($i = 0; $i < 50; $i++) {
    $objects[] = new ArrayObject([$i]);
}
$ex = \Rapira\get_dispatcher()->receive();
$ex->writeBody((string) getmypid());
