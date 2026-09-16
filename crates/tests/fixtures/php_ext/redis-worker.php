<?php
$handler = static function (): void {
	if (!extension_loaded('redis')) {
		echo 'skip';
		return;
	}
	$redis = new Redis();
	$redis->setOption(Redis::OPT_SERIALIZER, Redis::SERIALIZER_IGBINARY);
	$value = ['message' => 'rapira', 'count' => 42];
	$serialized = $redis->_serialize($value);
	echo 'redis:' . (
		$serialized === igbinary_serialize($value) && $redis->_unserialize($serialized) === $value
		? 'ok' : 'fail'
	);
};
while (\Rapira\handle_request($handler)) {
}
