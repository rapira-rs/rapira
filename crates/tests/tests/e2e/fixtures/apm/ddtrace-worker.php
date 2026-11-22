<?php
// The compatible-SAPI check (ext/datadog.c, names in components/sapi/sapi.c) lists neither "fastcgi" (8.4) nor "rapira" (8.5), so MINIT disables tracing and still returns SUCCESS.
// https://docs.datadoghq.com/tracing/trace_collection/compatibility/php/
$app = bin2hex(random_bytes(8));
$handler = static function () use ($app): void {
	// phpinfo() renders the disable flag as the "Datadog tracing support" row; ini_get('ddtrace.disable') does not show it
	ob_start();
	phpinfo(INFO_MODULES);
	$info = (string) ob_get_clean();
	// tags and the text-mode " => " become spaces, so the row parses the same in both phpinfo modes
	$flat = (string) preg_replace('/<[^>]*>/', ' ', $info);
	$flat = trim((string) preg_replace('/\s+/', ' ', str_replace('=>', ' ', $flat)));
	if (str_contains($flat, 'Datadog tracing support disabled (not built)')) {
		$state = 'not-built';
	} elseif (str_contains($flat, 'Datadog tracing support disabled')) {
		$state = 'disabled';
	} elseif (str_contains($flat, 'Datadog tracing support enabled')) {
		$state = 'enabled';
	} else {
		$state = 'unknown';
	}
	$expected = PHP_VERSION_ID >= 80500 ? 'rapira' : 'fastcgi';
	echo implode("\n", [
		'dd:sapi=' . PHP_SAPI,
		'dd:sapi_ok=' . (PHP_SAPI === $expected ? '1' : '0'),
		'dd:version=' . phpversion('ddtrace'),
		'dd:tracing=' . $state,
		'dd:active_span=' . (function_exists('DDTrace\active_span')
			? var_export(\DDTrace\active_span(), true) : 'n/a'),
		'dd:app=' . $app,
	]);
};
while (\Rapira\handle_request($handler)) {
}
