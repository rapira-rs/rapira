<?php
$handler = static function (): void {
	if (!extension_loaded('imap')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		imap_timeout('boom');
		return;
	}
	// c-client rejects the empty host before any DNS or socket work (uw-imap mail.c). If this is not true, the 1-second timeout limits the stall of the suite.
	imap_timeout(IMAP_OPENTIMEOUT, 1);
	$open = @imap_open('{}INBOX', 'u', 'p');
	// imap_errors() empties the stack. RSHUTDOWN reports each entry left on the stack as an E_NOTICE.
	$errs = imap_errors();
	$rejected = is_array($errs) && str_contains(implode('|', $errs), 'invalid remote specification');
	echo 'imap:' . ($open === false && $rejected ? 'ok' : 'bad');
};
while (\Rapira\handle_request($handler)) {
}
