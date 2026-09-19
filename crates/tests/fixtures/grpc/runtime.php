<?php

$dispatcher = Rapira\get_dispatcher();
$services = $dispatcher->getServices();
$methods = array_map(fn($method) => [$method->name, $method->kind->value], $services[0]->methods);
try {
    while ($call = $dispatcher->receive()) {
        $context = $call->getContext();
        $tls = $context->tls;
        $call->getResponseMetadata()->addBinaryTrailer('x-reply-bin', $call->getMessage());
        $call->respond(json_encode([
            $methods,
            [$context->method, $context->remote->path, $context->deadline, $context->receivedAt],
            [get_class($tls), $tls->version, $tls->cipher, $tls->negotiatedProtocol, $tls->requestedServerName, $tls->certSerial, $tls->certOrganization, $tls->certFingerprint],
            bin2hex($context->metadata->values('x-bytes-bin')[0]),
        ], JSON_THROW_ON_ERROR));
    }
} catch (Rapira\Exception\ClosedException) {
}
