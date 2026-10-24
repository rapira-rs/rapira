<?php
// Frameworks keep the declared length on HEAD and send no body, as Symfony Response::prepare() does.
header('Content-Length: 11');
if ($_SERVER['REQUEST_METHOD'] !== 'HEAD') {
    echo 'hello world';
}
