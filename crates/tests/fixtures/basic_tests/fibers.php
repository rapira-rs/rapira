<?php
function fib(int $n): int
{
    return $n < 2 ? $n : fib($n - 1) + fib($n - 2);
}

// 300 independent fibers suspend twice each. Each start and each resume crosses the boundary between the fiber stack and the worker stack, where PHP swaps EG(stack_base) and EG(stack_limit) in and back out
$sum = 0;
for ($i = 0; $i < 300; $i++) {
    $f = new Fiber(function (): int {
        $a = fib(14);                 // 377, from recursion on the fiber's own stack
        $b = Fiber::suspend($a);      // switches to the worker stack, resumes with 377
        $c = Fiber::suspend($b + 1);  // switches to the worker stack, resumes with 378
        return $a + $c;               // 755
    });
    $r1 = $f->start();                // 377, the first suspend value
    $r2 = $f->resume($r1);            // 378, the second suspend value
    $f->resume($r2);
    $sum += $f->getReturn();          // 755 each
}
// sum = 300 * 755 = 226500

// 25 nested fibers are alive at the same time, so PHP saves the stack base and limit 25 times on the way in and restores them 25 times on the way out
function nest(int $depth): int
{
    if ($depth === 0) {
        return fib(12);               // 144
    }
    $inner = new Fiber(fn(): int => nest($depth - 1));
    $inner->start();                  // the inner fiber runs to its end and never suspends
    return $inner->getReturn();
}
$sum += nest(25);                     // +144

// a stale stack base from any fiber boundary faults on this last compile and echo
echo "fibers ok sum=$sum\n";          // 226644