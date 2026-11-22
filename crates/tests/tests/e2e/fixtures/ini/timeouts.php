<?php

while (\Rapira\handle_request(static function () {
    echo json_encode([
        ini_get('max_execution_time'),
        ini_get('max_input_time'),
        ini_set('max_execution_time', '7'),
        set_time_limit(6),
        ini_get('max_execution_time'),
    ]);
})) {}
