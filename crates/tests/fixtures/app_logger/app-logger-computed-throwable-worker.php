<?php

// Logs a Throwable whose message the engine computes into a temporary zval, then writes the memory_get_usage() growth of 1000 more log calls.

class GetMessage extends \Exception
{
    public function __construct()
    {
        parent::__construct();
        unset($this->message); // a read of message now calls __get
    }

    public function __get($name)
    {
        // Built at run time: opcache folds a constant string into an interned string, which has no refcount to leak.
        return "computed $name";
    }
}

class HookedMessage extends \Exception
{
    protected $message {
        get => '[app] ' . $this->message;
    }
}

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        parse_str(parse_url($ex->getRequest()->target, PHP_URL_QUERY) ?: '', $q);
        $e = match ($q['case'] ?? '') {
            'get' => new GetMessage(),
            'hook' => new HookedMessage('m'),
        };
        // Warm-up: the first call allocates state that later calls reuse.
        \Rapira\log('probe', \Rapira\LogLevel::Error, ['e' => $e]);
        $before = memory_get_usage();
        for ($i = 0; $i < 1000; $i++) {
            \Rapira\log('probe', \Rapira\LogLevel::Error, ['e' => $e]);
        }
        $ex->writeBody((string) (memory_get_usage() - $before));
    }
} catch (\Rapira\Exception\ClosedException) {
}
