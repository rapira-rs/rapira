<?php
// a non-fatal warning does not bail out, so no recycle hides the reset. This proves that rapira_clear_last_error() clears PG(last_error_message) between worker units
$handler = static function (): void {
    if (($_GET['step'] ?? '') === 'warn') {
        trigger_error('leaky', E_USER_WARNING);
        echo 'warned';
        return;
    }
    $e = error_get_last();
    echo $e === null ? 'clean' : 'leaked:' . $e['message'];
};
while (\Rapira\handle_request($handler)) {
}
