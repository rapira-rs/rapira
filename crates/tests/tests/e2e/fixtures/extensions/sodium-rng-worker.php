<?php
// Each worker caches one randombytes_buf() draw taken right after the fork; no two workers may return the same draw.

use Rapira\Exception\ClosedException;

$body = extension_loaded('sodium')
	? 'pid=' . getmypid() . ' first=' . bin2hex(sodium_crypto_shorthash_keygen())
	: 'skip';
$d = \Rapira\get_dispatcher();
try {
	while (true) {
		$ex = $d->receive();
		$ex->writeBody($body);
	}
} catch (ClosedException) {
}
