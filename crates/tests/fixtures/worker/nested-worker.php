<?php
// the nesting guard must refuse before the inner call can take a unit or rebind the current request.
$handler = static function (): void {
    try {
        \Rapira\handle_request(static function (): void {});
        echo 'inner-returned';
    } catch (\Error $e) {
        echo 'nested: ', $e->getMessage();
    }
};
while (\Rapira\handle_request($handler)) {
}
