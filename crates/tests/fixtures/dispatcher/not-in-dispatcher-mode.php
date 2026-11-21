<?php

try {
    \Rapira\get_dispatcher();
    echo "returned\n";
} catch (\Rapira\Exception\NoDispatcherError $e) {
    echo 'class: ', $e::class, "\n";
    echo 'rapira: ', $e instanceof \Rapira\Exception\RapiraThrowable ? 'yes' : 'no', "\n";
    echo 'message: ', $e->getMessage(), "\n";
}

// App code can construct the RuntimeException family, and catch (\RuntimeException) must still catch it.
try {
    throw new \Rapira\Exception\TimeoutException('elapsed');
} catch (\RuntimeException $e) {
    echo 'timeout-as-runtime: ', $e instanceof \Rapira\Exception\RapiraThrowable ? 'yes' : 'no', "\n";
}
echo 'done';
