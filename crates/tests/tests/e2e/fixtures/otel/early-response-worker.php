<?php

while (Rapira\handle_request(static function (): void {
    $gate = stream_socket_client('tcp://' . $_SERVER['HTTP_X_GATE'], timeout: 20);
    if ($gate === false) {
        throw new RuntimeException('gate connection failed');
    }
    stream_set_timeout($gate, 20);
    echo 'early response';
    rapira_finish_request();
    if (fread($gate, 1) !== '1') {
        throw new RuntimeException('gate was not released');
    }
    fclose($gate);
})) {
}
