<?php
// Each case answers one line. The table in objects.rs holds the expected line.

interface Marked {}
class Leak {}
class SubLeak extends Leak implements Marked {}
class Node { public ?Node $peer = null; }
class Lazy { public int $value = 1; }
class Unused {}

// A static property holds the objects of a case, as a leak does. The optimizer can remove an object that only an unread local holds.
final class Held { public static array $objects = []; }

// isset() and offsetExists() return false for a null value, so has() walks the map.
function has(\WeakMap $map, object $wanted): bool
{
    foreach ($map as $object => $_) {
        if ($object === $wanted) {
            return true;
        }
    }
    return false;
}

function rejected(array $classes): string
{
    try {
        \Rapira\get_objects($classes);
        return 'no error';
    } catch (\TypeError $e) {
        return $e->getMessage();
    }
}

$cases = [
    'count-per-class' => static function (): string {
        Held::$objects = [new Leak(), new Leak(), new SubLeak()];
        $counts = \Rapira\count_objects();
        return 'Leak=' . $counts[Leak::class] . ' SubLeak=' . $counts[SubLeak::class];
    },
    'count-no-instance' => static function (): string {
        return array_key_exists(Unused::class, \Rapira\count_objects()) ? 'present' : 'absent';
    },
    'count-cycle' => static function (): string {
        $a = new Node();
        $b = new Node();
        $a->peer = $b;
        $b->peer = $a;
        Held::$objects = [$a];
        unset($a, $b);
        // Only the cycle holds the pair after this line.
        Held::$objects = [];
        $garbage = \Rapira\count_objects()[Node::class] ?? 0;
        gc_collect_cycles();
        return 'garbage=' . $garbage . ' collected=' . (\Rapira\count_objects()[Node::class] ?? 0);
    },
    'count-anonymous' => static function (): string {
        Held::$objects = [new class {}];
        return (string) (\Rapira\count_objects()[Held::$objects[0]::class] ?? 0);
    },
    'get-all' => static function (): string {
        $count = array_sum(\Rapira\count_objects());
        $map = \Rapira\get_objects();
        return 'counted=' . ($count === count($map) ? 'same' : $count . '/' . count($map))
            . ' self=' . (has($map, $map) ? 'yes' : 'no');
    },
    'get-instanceof' => static function (): string {
        Held::$objects = [new Leak(), new SubLeak()];
        $byClass = \Rapira\get_objects([Leak::class]);
        $byInterface = \Rapira\get_objects([Marked::class]);
        return 'class=' . count($byClass) . ' interface=' . count($byInterface)
            . ' sub=' . (has($byInterface, Held::$objects[1]) ? 'yes' : 'no');
    },
    'get-any-of' => static function (): string {
        Held::$objects = [new Leak(), new SubLeak()];
        return 'matched=' . count(\Rapira\get_objects([Unused::class, Marked::class]));
    },
    'get-values' => static function (): string {
        Held::$objects = [new Leak(), new SubLeak()];
        $types = [];
        foreach (\Rapira\get_objects([Leak::class]) as $value) {
            $types[] = get_debug_type($value);
        }
        return implode(',', $types);
    },
    'get-unloaded-name' => static function (): string {
        $asked = [];
        $loader = static function (string $class) use (&$asked): void {
            $asked[] = $class;
        };
        spl_autoload_register($loader);
        $map = \Rapira\get_objects(['NotLoaded\\Thing']);
        spl_autoload_unregister($loader);
        return 'matched=' . count($map) . ' autoloaded=' . count($asked);
    },
    'get-weak' => static function (): string {
        Held::$objects = [new Leak()];
        $map = \Rapira\get_objects([Leak::class]);
        $held = count($map);
        Held::$objects = [];
        return 'held=' . $held . ' released=' . count($map);
    },
    'get-lazy' => static function (): string {
        $reflector = new ReflectionClass(Lazy::class);
        Held::$objects = [$reflector->newLazyGhost(static function (Lazy $object): void {})];
        $map = \Rapira\get_objects([Lazy::class]);
        return 'matched=' . count($map) . ' '
            . ($reflector->isUninitializedLazyObject(Held::$objects[0]) ? 'uninitialized' : 'initialized');
    },
    'foreach-over-object' => static function (): string {
        $classes = [];
        // The engine keeps its own iterator object in the store for the time of this loop.
        foreach ((static function (): \Generator { yield 1; })() as $_) {
            $classes = array_keys(\Rapira\count_objects());
            foreach (\Rapira\get_objects() as $object => $_value) {
                $classes[] = $object::class;
            }
        }
        $unknown = array_filter($classes, static fn (string $class): bool => !class_exists($class, false));
        return $unknown === [] ? 'registered' : implode(',', array_unique($unknown));
    },
    'get-object-entry' => static fn (): string => rejected([new Leak()]),
    'get-int-entry' => static fn (): string => rejected([Leak::class, 42]),
];

$handler = static function () use ($cases): void {
    echo $cases[$_GET['case']]();
    Held::$objects = [];
};
while (\Rapira\handle_request($handler)) {
}
