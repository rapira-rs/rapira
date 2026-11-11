<?php
// A session store without a lock whose write() takes 200 ms. ?set=V stores V;
// other requests read the session without a write. The body is "<pid> <value>".
// REGISTER_SHUTDOWN=1 passes register_shutdown = true to session_set_save_handler().
final class UnlockedStore implements SessionHandlerInterface
{
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
		return (string) @file_get_contents(__DIR__ . "/sess_$id");
	}

	public function write(string $id, string $data): bool
	{
		usleep(200_000);
		return file_put_contents(__DIR__ . "/sess_$id", $data) !== false;
	}

	public function destroy(string $id): bool
	{
		return @unlink(__DIR__ . "/sess_$id");
	}

	public function gc(int $max_lifetime): int|false
	{
		return 0;
	}
}

$serve = static function (): void {
	session_id('shared');
	if (isset($_GET['set'])) {
		session_start();
		$_SESSION['v'] = $_GET['set'];
	} else {
		session_start(['read_and_close' => true]);
	}
	echo getmypid(), ' ', $_SESSION['v'] ?? 'none';
};

session_set_save_handler(new UnlockedStore(), getenv('REGISTER_SHUTDOWN') === '1');
if (\Rapira\get_mode() === \Rapira\Mode::Worker) {
	while (\Rapira\handle_request($serve)) {
	}
} else {
	$serve();
}
