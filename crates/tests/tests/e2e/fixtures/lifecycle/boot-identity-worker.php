<?php

// Reads the server identity at boot, before the first receive(), and answers every unit with it.
$d = \Rapira\get_dispatcher();
$info = $d->getInfo();
$boot = sprintf('name=%s port=%d', $info->serverName(), $info->serverPort());
\Rapira\log('boot-identity', context: ['result' => $boot]);
while (true) {
    $d->receive()->writeBody($boot);
}
