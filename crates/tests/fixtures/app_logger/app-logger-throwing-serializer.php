<?php

// An exception thrown inside the jsonSerialize() of a context value must not escape log() or end the cycle.
final class Bomb implements \JsonSerializable
{
	public function jsonSerialize(): mixed
	{
		throw new \RuntimeException('serializer bomb');
	}
}

try {
	\Rapira\log('bombed', \Rapira\LogLevel::Error, ['keep' => 'visible', 'bomb' => new Bomb()]);
	echo 'logged';
} catch (\Throwable $e) {
	echo 'escaped:', $e->getMessage();
}
