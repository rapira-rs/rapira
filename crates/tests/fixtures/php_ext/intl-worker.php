<?php
$handler = static function (): void {
	if (!extension_loaded('intl')) {
		echo 'skip';
		return;
	}
	echo 'intl:' . Normalizer::normalize("cafe\u{0301}", Normalizer::FORM_C);
};
while (\Rapira\handle_request($handler)) {
}
