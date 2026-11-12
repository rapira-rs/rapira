<?php
// Each release folder of the deploy test gets a copy; release.php and release.txt name the release.
use Rapira\Exception\ClosedException;
use Rapira\Http\Exception\FileNotSendableException;

$release = require __DIR__ . '/release.php';
$dispatcher = \Rapira\get_dispatcher();
try {
    while (true) {
        $exchange = $dispatcher->receive();
        $exchange->writeBody("code=$release cwd=" . getcwd() . ' file=', eos: false);
        try {
            $exchange->sendFile(__DIR__ . '/release.txt');
        } catch (FileNotSendableException $e) {
            $exchange->writeBody($e->getMessage());
        }
    }
} catch (ClosedException) {
}
