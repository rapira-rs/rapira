<?php
// One line per name: "yes" when the class table holds it. class_exists() is false for an interface, so interface_exists() covers the interfaces.
$handler = static function (): void {
    header('Content-Type: text/plain');
    foreach ([
        'Rapira\Work',
        'Rapira\Http\Exchange',
        'Rapira\Internal\Http\Exchange',
        'Rapira\Grpc\UnaryCall',
        'Rapira\Internal\Grpc\UnaryCall',
    ] as $name) {
        echo $name, ': ', (class_exists($name, false) || interface_exists($name, false)) ? 'yes' : 'no', "\n";
    }
    echo 'Rapira\Internal\Http\Exchange extends Rapira\Work: ',
        is_subclass_of('Rapira\Internal\Http\Exchange', 'Rapira\Work') ? 'yes' : 'no', "\n";
};
while (\Rapira\handle_request($handler)) {
}
