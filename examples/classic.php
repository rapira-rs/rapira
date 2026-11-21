<?php
// Classic mode runs this file once per request, as a front controller.
header('content-type: text/plain');
echo 'classic: ', $_SERVER['REQUEST_METHOD'], ' ', $_SERVER['REQUEST_URI'], "\n";
