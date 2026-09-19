<?php

use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;
use Rapira\Grpc\ErrorDetail;
use Rapira\Grpc\GrpcDispatcher;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;

$dispatcher = \Rapira\get_dispatcher();
if (!$dispatcher instanceof GrpcDispatcher || $dispatcher->name() !== 'grpc') {
    throw new RuntimeException('gRPC dispatcher required');
}

$services = [];
foreach ($dispatcher->getServices() as $service) {
    $methods = [];
    foreach ($service->methods as $method) {
        $methods[] = [$method->name, $method->inputType, $method->outputType, $method->kind->value];
    }
    $services[] = [$service->name, $methods];
}
file_put_contents(__DIR__ . '/boot-' . getmypid() . '.json', json_encode($services, JSON_THROW_ON_ERROR));

try {
    while (true) {
        $call = $dispatcher->receive();
        $context = $call->getContext();
        $id = $context->metadata->values('x-id')[0] ?? '';
        file_put_contents(__DIR__ . '/calls', $context->method . ':' . $id . "\n", FILE_APPEND);

        try {
            $metadata = $call->getResponseMetadata();
            $metadata->addHeader('x-worker', (string) getmypid());
            $metadata->addHeader('x-protocol', $context->protocol->value);
            $metadata->addTrailer('x-result', 'completed');
            foreach ($context->metadata->values('x-repeat') as $value) {
                $metadata->addHeader('x-repeat', $value);
            }
            foreach ($context->metadata->values('x-data-bin') as $value) {
                $metadata->addBinaryTrailer('x-data-bin', $value);
            }

            if ($context->method === 'e2e.v1.Echo/Fail') {
                $call->fail(new Status(StatusCode::InvalidArgument, 'invalid % value', [
                    new ErrorDetail('type.googleapis.com/e2e.v1.Message', $call->getMessage()),
                ]));
            } else {
                if ($context->method === 'e2e.v1.Echo/Hold') {
                    file_put_contents(__DIR__ . '/active', $id);
                    while (!is_file(__DIR__ . '/release') && !$call->isCancelled()) {
                        usleep(1000);
                    }
                    if ($call->isCancelled()) {
                        file_put_contents(__DIR__ . '/cancelled', $id);
                    }
                }
                $call->respond($call->getMessage());
            }
        } catch (WorkDiscardedException) {
        }
    }
} catch (ClosedException) {
}
