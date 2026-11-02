<?php

// The classic twin of held-worker.php: logs each run and holds the unit until the release file exists.
\Rapira\log('held');
while (!file_exists($_GET['release'])) {
    usleep(10000);
}
echo 'done';
