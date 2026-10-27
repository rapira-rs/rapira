<?php

try {
    \Rapira\handle_request(static function (): void {});
    echo "returned\n";
} catch (\Rapira\Exception\NotInWorkerModeError $e) {
    echo 'class: ', $e::class, "\n";
    echo 'rapira: ', $e instanceof \Rapira\Exception\RapiraThrowable ? 'yes' : 'no', "\n";
    echo 'message: ', $e->getMessage(), "\n";
}

// ZPP runs before the mode gate: a non-callable is a TypeError everywhere.
try {
    \Rapira\handle_request('nope');
} catch (\TypeError) {
    echo "type-error\n";
}
echo 'done';
