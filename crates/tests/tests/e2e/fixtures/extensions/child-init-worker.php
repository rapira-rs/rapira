<?php
// Each worker answers with the number of child_init_func calls that zend_test counted in its process.

use Rapira\Exception\ClosedException;

$body = function_exists('zend_test_child_init_count')
	? 'count=' . zend_test_child_init_count()
	: 'skip';
$d = \Rapira\get_dispatcher();
try {
	while (true) {
		$ex = $d->receive();
		$ex->writeBody($body);
	}
} catch (ClosedException) {
}
