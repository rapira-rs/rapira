<?php

// Probe toggles ride on the request target: this mode has no superglobals.

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Http\Exception\ContentLengthExceededError;

$d = \Rapira\get_dispatcher();
try {
	while (true) {
		$ex = $d->receive();
		$req = $ex->getRequest();
		parse_str(parse_url($req->target, PHP_URL_QUERY) ?: "", $q);
		$probe = $q["probe"] ?? "";

		if ($probe === "sse") {
			$ex->writeHead(200, ["content-type" => ["text/event-stream"]]);
			$ex->flush();
			usleep(400_000);
			$ex->writeBody("data: one\n\n", eos: false);
			usleep(100_000);
			$ex->writeBody("data: two\n\n", eos: true);
			continue;
		}
		if ($probe === "chunks") {
			$ex->writeBody("alpha,", eos: false);
			$ex->writeBody("beta", eos: true);
			continue;
		}
		if ($probe === "interim") {
			$ex->writeHead(103, ["link" => ["</app.css>; rel=preload"]]);
			$ex->writeHead(200, ["content-type" => ["text/plain"]]);
			$ex->writeBody("hello");
			continue;
		}
		if ($probe === "cl-slow-body") {
			$ex->writeHead(200, ["content-length" => ["5"]]);
			$ex->flush();
			usleep(2_000_000);
			$ex->writeBody("01234");
			continue;
		}
		if ($probe === "cl-exceeded") {
			// tens copies of "0123456789" under a declared length of cl bytes
			$ex->writeHead(200, ["content-length" => [$q["cl"] ?? "5"]]);
			try {
				$ex->writeBody(str_repeat("0123456789", (int) ($q["tens"] ?? 1)));
			} catch (ContentLengthExceededError) {
			}
			continue;
		}
		if ($probe === "discard") {
			$ex->writeHead(200);
			$ex->flush(); // the test reads this head, then walks away
			usleep(300_000);
			try {
				$ex->writeBody("x", eos: false);
				$ex->writeBody("", eos: true);
			} catch (WorkDiscardedException $e) {
				\Rapira\log("discarded", context: ["class" => $e::class]);
			}
			continue;
		}
		if ($probe === "trailers") {
			$ex->writeBody("payload", eos: false);
			$ex->writeTrailers(["x-checksum" => ["abc123"]]);
			continue;
		}
		if ($probe === "sendfile") {
			$ex->sendFile($req->headers["x-path"][0] ?? "");
			continue;
		}
		if ($probe === "die-mid-stream") {
			$ex->writeBody("first,", eos: false);
			exit(0);
		}
		$ex->writeBody("ok");
	}
} catch (ClosedException) {
}
