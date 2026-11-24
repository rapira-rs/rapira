<?php

// A spin uses wall-clock time and CPU time. A sleep uses wall-clock time and no CPU time.
if (isset($_GET['spin'])) {
    while (true) {
    }
}
usleep((int) ($_GET['sleep'] ?? 0) * 1000);
echo 'done';
