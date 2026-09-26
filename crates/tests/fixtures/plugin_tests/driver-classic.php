<?php
// Classic-mode entrypoint for the plugin tests: each exchange runs this script fresh.
echo 'ok:' . ($_GET['from'] ?? '?');
