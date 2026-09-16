<?php

while (\Rapira\handle_request(static function (): void {
    require __DIR__ . '/construct.php';
})) {
}
