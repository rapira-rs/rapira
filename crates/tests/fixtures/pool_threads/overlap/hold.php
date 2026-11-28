<?php

function hold_until_release(?Closure $sample = null): void
{
    $deadline = hrtime(true) + 30_000_000_000;
    while (!file_exists(__DIR__ . '/release')) {
        if (hrtime(true) >= $deadline) {
            throw new RuntimeException('No release from the client');
        }
        if ($sample !== null) {
            $sample();
        }
        usleep(10_000);
    }
}
