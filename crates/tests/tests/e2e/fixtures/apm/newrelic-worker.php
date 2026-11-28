<?php
// The SAPI name is not "cli", so the agent takes the web path. It must load, register its API and leave the request cycle unchanged with no daemon and no valid license.
$app = bin2hex(random_bytes(8));
$handler = static function () use ($app): void {
	if (($_GET['boom'] ?? '') === '1') {
		throw new RuntimeException('newrelic-worker: uncaught');
	}
	$api = ['newrelic_start_transaction', 'newrelic_end_transaction', 'newrelic_name_transaction',
		'newrelic_add_custom_parameter', 'newrelic_notice_error', 'newrelic_record_custom_event'];
	foreach ($api as $f) {
		if (!function_exists($f)) {
			echo 'nr:missing:' . $f;
			return;
		}
	}
	newrelic_name_transaction('rapira-e2e');
	newrelic_add_custom_parameter('rapira', 'e2e');
	newrelic_notice_error('rapira e2e probe');
	echo 'nr:ok:' . phpversion('newrelic') . ':' . $app;
};
while (\Rapira\handle_request($handler)) {
}
