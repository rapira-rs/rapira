<?php
// Each release folder of the deploy test gets a copy. It prints the script paths of $_SERVER.
header('Content-Type: text/plain');
printf('file=%s root=%s name=%s self=%s', $_SERVER['SCRIPT_FILENAME'], $_SERVER['DOCUMENT_ROOT'], $_SERVER['SCRIPT_NAME'], $_SERVER['PHP_SELF']);
