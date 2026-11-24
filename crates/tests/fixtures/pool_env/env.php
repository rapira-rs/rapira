<?php
// The env routes of the pool_env tests: /put writes with putenv() and then reads, /child reads the env of a child process, /ini reads ${NAME} in INI text, /shape reads names that the env arrays skip or key as integers, and every other path reads.

const ENV_NAMES = ['POOL_PROBE', 'POOL_INHERITED', 'POOL_EMPTY', 'POOL_ONLY', 'POOL_DROP', 'POOL_NEW'];

// getenv() of each name, and the names for which getenv($name, true) or the getenv() array gives another value.
function env_view(): array
{
    $all = getenv();
    $view = [];
    $mismatch = [];
    foreach (ENV_NAMES as $name) {
        $view[$name] = getenv($name);
        if (getenv($name, true) !== $view[$name] || ($all[$name] ?? false) !== $view[$name]) {
            $mismatch[] = $name;
        }
    }
    return ['view' => $view, 'mismatch' => $mismatch];
}

function env_put(): array
{
    $put = putenv('POOL_PROBE=put') && putenv('POOL_ONLY=') && putenv('POOL_DROP') && putenv('POOL_NEW=a=b');
    return env_view() + ['put' => $put];
}

function env_child(): array
{
    exec('env', $lines);
    $seen = [];
    foreach ($lines as $line) {
        [$name, $value] = explode('=', $line, 2) + [1 => false];
        $seen[$name] = $value;
    }
    $out = [];
    foreach (ENV_NAMES as $name) {
        $out[$name] = $seen[$name] ?? false;
    }
    return $out;
}

// ${NAME} of each name in INI text. A name that no layer defines gives the fallback.
function env_ini(): array
{
    $out = [];
    foreach (ENV_NAMES as $name) {
        $out[$name] = parse_ini_string('value = ${' . $name . ':-undefined}')['value'];
    }
    return $out;
}

function env_shape(array $bootServer): array
{
    $calls = [
        putenv('POOL.PUT=put'),
        putenv('4343=num'),
        putenv(assignment: 'POOL_NAMED=named'),
        getenv(name: 'POOL_NAMED'),
        is_array(getenv(null)),
        is_array(getenv(local_only: true)),
        getenv(''),
    ];
    foreach (['', '=v'] as $bad) {
        try {
            $calls[] = putenv($bad);
        } catch (\ValueError $e) {
            $calls[] = $e->getMessage();
        }
    }
    $odd = ['POOL.DOT', 'POOL SPACE', 'POOL[BR', 'POOL.CONF', 'POOL.PUT'];
    $out = ['calls' => $calls, 'named' => []];
    foreach ([...$odd, '4242', '4343', '4444'] as $name) {
        $out['named'][] = getenv($name);
    }
    foreach (['getenv' => getenv(), 'env' => $_ENV, 'server' => $bootServer] as $key => $array) {
        $ints = array_values(array_filter(array_keys($array), 'is_int'));
        sort($ints);
        $out[$key] = ['odd' => array_values(array_intersect($odd, array_keys($array))), 'int' => $ints];
    }
    return $out;
}

function env_route(string $path, array $bootServer = []): array
{
    return match ($path) {
        '/put' => env_put(),
        '/child' => env_child(),
        '/ini' => env_ini(),
        '/shape' => env_shape($bootServer),
        default => env_view(),
    };
}
