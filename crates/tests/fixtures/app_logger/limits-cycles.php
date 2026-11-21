<?php

// The cases of the Monolog tests testIgnoresRecursiveObjectReferences and testCanNormalizeReferences. Both reference cycles must break with no diagnostic and no loss of siblings.
$foo = new \stdClass();
$bar = new \stdClass();
$foo->bar = $bar;
$bar->foo = $foo;

$x = ['foo' => 'bar'];
$y = ['x' => &$x];
$x['y'] = &$y;

\Rapira\log('cycles', \Rapira\LogLevel::Error, [
    'objects' => $foo,
    'arrays' => $y,
    'keep' => 'visible',
]);

echo 'logged';
