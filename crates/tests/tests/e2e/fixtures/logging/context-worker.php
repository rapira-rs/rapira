<?php

final class ContextValue implements \JsonSerializable
{
    public static bool $serialized = false;

    public function jsonSerialize(): string
    {
        self::$serialized = true;
        return 'value';
    }
}

$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        ContextValue::$serialized = false;
        \Rapira\log('context-filter', \Rapira\LogLevel::Info, ['item' => new ContextValue()]);
        $exchange->writeHead(200, ['content-type' => ['text/plain']]);
        $exchange->writeBody(ContextValue::$serialized ? 'serialized' : 'skipped');
    }
} catch (\Rapira\Exception\ClosedException) {
}
