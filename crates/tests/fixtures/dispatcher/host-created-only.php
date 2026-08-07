<?php

try {
    new \Rapira\Internal\Http\Dispatcher();
    echo "constructed\n";
} catch (\Error $e) {
    // the engine refuses the private constructor before its body could
    echo 'blocked: ', $e->getMessage(), "\n";
}
echo 'done';
