<?php
$dir = getenv('PHPRC');
touch($dir . '/preloading');
while (!is_file($dir . '/release-preload')) {
    usleep(10_000);
}
