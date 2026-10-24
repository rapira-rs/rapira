<?php
class Track
{
	public static int $served = 0;
	public static int $done = 0;
}

// the test drops the receiver after 'held'; the sleep lets the drop land before the write
function write_after_leave(): void
{
	\Rapira\log('held');
	usleep(300000);
	echo "payload\n"; // aborted write: the SAPI ends the unit
	Track::$done++; // must not run when the client disconnected
}

final class WriteOnDestruct
{
	public function __destruct()
	{
		write_after_leave();
	}
}

$handler = static function (): void {
	Track::$served++;
	header('Content-Type: text/plain');
	switch ($_GET['case'] ?? '') {
		case 'probe':
			// (int): php-src master returns bool, PHP 8 returns int. https://github.com/php/php-src/commit/8639e7522a33309b1ed9d096c4a90204e2e50d1a
			echo "served=", Track::$served, " done=", Track::$done, " aborted=", (int) connection_aborted();
			return;
		case 'echo':
			write_after_leave();
			return;
		case 'flush':
			// the buffer reaches the SAPI only in the teardown flush
			ob_start();
			\Rapira\log('held');
			usleep(300000);
			echo "payload\n";
			return;
		case 'shutdown':
			register_shutdown_function('write_after_leave');
			return;
		case 'destruct':
			$o = new WriteOnDestruct();
			return;
	}
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
