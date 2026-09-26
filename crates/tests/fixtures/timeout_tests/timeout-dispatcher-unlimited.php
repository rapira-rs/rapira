<?php

// the first receive() captures 0 as the per-unit budget of this cycle
set_time_limit(0);
require __DIR__ . '/timeout-dispatcher.php';
