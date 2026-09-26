<?php
// Echoes what repeated request fields look like to PHP once the plugin has combined them.
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo ($_COOKIE['a'] ?? '-'), ',', ($_COOKIE['b'] ?? '-'), "\n";
    echo $_SERVER['HTTP_COOKIE'] ?? '-', "\n";
    echo $_SERVER['HTTP_X_FORWARDED_FOR'] ?? '-', "\n";
};
while (\Rapira\handle_request($handler)) {
}
