<?php
$handler = static function (): void {
    header('content-type: application/json');
    echo json_encode(array_map(null, array_keys($_SERVER), array_values($_SERVER)));
};
while (\Rapira\handle_request($handler)) {
}
