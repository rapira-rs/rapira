<?php
// A fatal error ends every app instance at its first request.
$handler = static function (): void {
    ini_set('display_errors', '0'); // no error text in the body: nothing precedes the 500
    trigger_error('boot bomb', E_USER_ERROR); // E_USER_ERROR -> zend_bailout
};
while (\Rapira\handle_request($handler)) { gc_collect_cycles(); }
