<?php
// The fatal request sends no output, so the header send at the request end runs the callback, and its fatal bails out there.

if (isset($_GET['fatal'])) {
    header_register_callback(static function (): void {
        str_repeat('x', 1 << 40);
    });
    return;
}
echo 'ok';
