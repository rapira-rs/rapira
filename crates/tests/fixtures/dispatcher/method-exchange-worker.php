<?php

declare(strict_types=1);

// The unit reaches a method as a call argument, and the state verbs feed a branch there.
final readonly class Server
{
    public function __construct(private \Rapira\Http\HttpDispatcher $dispatcher)
    {
    }

    public function run(): void
    {
        try {
            while (true) {
                $this->serve($this->dispatcher->receive());
            }
        } catch (\Rapira\Exception\ClosedException) {
        }
    }

    private function serve(\Rapira\Http\Exchange $exchange): void
    {
        if ($exchange->isCancelled() || $exchange->isFinalized()) {
            return;
        }
        $exchange->writeBody(sprintf(
            'cancelled=%s finalized=%s',
            var_export($exchange->isCancelled(), true),
            var_export($exchange->isFinalized(), true),
        ));
    }
}

(new Server(\Rapira\get_dispatcher()))->run();
