<?php
class PreLoopHandler implements \SessionHandlerInterface
{
	private static array $data = [];

	public function open(string $path, string $name): bool
	{
		return true;
	}

	public function close(): bool
	{
		return true;
	}

	public function read(string $id): string|false
	{
		return self::$data[$id] ?? '';
	}

	public function write(string $id, string $data): bool
	{
		self::$data[$id] = $data;
		return true;
	}

	public function destroy(string $id): bool
	{
		unset(self::$data[$id]);
		return true;
	}

	public function gc(int $max_lifetime): int|false
	{
		return 0;
	}

	// PHP 8.6 deprecates save handlers without create_sid() and validateId(). PHP 9.0 requires them.
	public function create_sid(): string
	{
		return bin2hex(random_bytes(8));
	}

	public function validateId(string $id): bool
	{
		return isset(self::$data[$id]);
	}
}

session_set_save_handler(new PreLoopHandler(), true);
$n = 0;
$handler = static function () use (&$n): void {
	if (($_GET['action'] ?? 'check') === 'check') {
		$mode = ini_get('session.save_handler');
		echo "save_handler=", $mode, "\n", $mode === 'user' ? 'HANDLER_PRESERVED' : 'HANDLER_LOST';
		return;
	}
	$err = '';
	set_error_handler(static function (int $no, string $msg) use (&$err): bool {
		$err .= $msg;
		return true;
	});
	try {
		session_id('preloop-' . ++$n);
		if (session_start()) {
			$_SESSION['test'] = 'v' . $n;
			session_write_close();
			echo "SESSION_OK";
		} else {
			echo "SESSION_START_FAILED";
		}
	} catch (\Throwable $e) {
		echo "EXCEPTION:", $e->getMessage();
	}
	restore_error_handler();
	if ($err !== '') {
		echo " ERROR:", $err;
	}
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
