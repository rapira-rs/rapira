<?php
// a bounded worker. The entrypoint returns while the intake is still open, so the cycle must classify as Recycle and run the boot again.
\Rapira\handle_request(static function (): void {
    echo 'once';
});
\Rapira\log('one-turn-done');
