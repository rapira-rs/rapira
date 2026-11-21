<?php
// The classic-mode entrypoint of the plugin tests. Each exchange runs this entrypoint in a new cycle.
echo 'ok:' . ($_GET['from'] ?? '?');
