<?php
$handler = static function (): void {
    [$post, $files] = request_parse_body();
    foreach ($post as $name => $value) {
        echo "post $name ", strlen($value), "\n";
    }
    foreach ($files as $name => $file) {
        echo "file $name {$file['error']} {$file['size']}\n";
    }
};
while (\Rapira\handle_request($handler)) {
    gc_collect_cycles();
}
