<?php
// the engine fills PG(last_error_*) for any mask, so only the teardown log can still apply error_reporting(). The boot sets it, because worker mode restores INI per cycle, not per unit
error_reporting(E_ALL & ~E_DEPRECATED & ~E_USER_DEPRECATED);

$handler = static function (): void {
	switch ($_GET['step'] ?? '') {
		case 'deprecated':
			trigger_error('MASKED-DEPRECATION', E_USER_DEPRECATED);
			break;
		case 'warn':
			trigger_error('REPORTED-WARNING', E_USER_WARNING);
			break;
		case 'boom':
			// an uncaught throw reaches php_error_cb as E_ERROR with no bailout, and trigger_error(E_USER_ERROR) would emit its own deprecation on 8.4+
			throw new \RuntimeException('REPORTED-FATAL');
		case 'silent-fatal':
			error_reporting(0);
			throw new \RuntimeException('SILENCED-FATAL');
		case 'shutdown':
			register_shutdown_function(static function (): void {
				throw new \RuntimeException('SHUTDOWN-UNCAUGHT');
			});
			break;
		case 'shutdown-handled':
			set_exception_handler(static function (\Throwable $e): void {
				echo ' handled ', $e->getMessage();
			});
			register_shutdown_function(static function (): void {
				throw new \RuntimeException('SHUTDOWN-HANDLED');
			});
			break;
		case 'logged':
			// error_reporting(E_ALL) unmasks it, and with log_errors the SAPI log callback reports it too
			error_reporting(E_ALL);
			ini_set('log_errors', '1');
			trigger_error('LOGGED-DEPRECATION', E_USER_DEPRECATED);
			break;
	}
	echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
