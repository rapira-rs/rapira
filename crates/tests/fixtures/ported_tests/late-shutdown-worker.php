<?php
// Two registrations in the boot and one after the loop. The cycle end runs the boot entries first, then the late one.
register_shutdown_function(static function (): void {
    \Rapira\log('sd boot-a');
});
register_shutdown_function(static function (): void {
    \Rapira\log('sd boot-b');
});
$handler = static function (): void {
    echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
register_shutdown_function(static function (): void {
    \Rapira\log('sd late');
});
