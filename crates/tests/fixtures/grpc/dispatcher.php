<?php

use Rapira\Grpc\ErrorDetail;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;

function errorClass(callable $operation): string {
    try {
        $operation();
        return 'accepted';
    } catch (Throwable $error) {
        return get_class($error);
    }
}

$dispatcher = Rapira\get_dispatcher();
$services = $dispatcher->getServices();
$method = $services[0]->methods[0];
$bootstrap = [Rapira\get_mode()->name, $dispatcher->name(), $services[0]->name, $method->name, $method->inputType, $method->outputType, $method->kind->value];
Rapira\log('grpc-ready');
$report = [];
try {
    while ($call = $dispatcher->receive()) {
        $case = $call->getContext()->metadata->values('test-case')[0];
        switch ($case) {
            case 'inspect':
                $context = $call->getContext();
                $metadata = $call->getResponseMetadata();
                $metadata->addHeader('X-Order', 'one');
                $snapshot = $metadata->headers();
                $metadata->addHeader('x-order', 'two');
                $metadata->addBinaryHeader('X-Binary-Bin', "\0\xff");
                $metadata->addTrailer('x-end', 'done');
                $call->__destruct();
                $result = [
                    'bootstrap' => $bootstrap,
                    'context' => [$context->method, $context->protocol->value, $context->receivedAt, $context->deadline, get_class($context->remote), $context->remote->ip, $context->remote->port, $context->tls, bin2hex($call->getMessage()), bin2hex($context->metadata->values('x-input-bin')[0])],
                    'snapshots' => [$snapshot->values('x-order'), $metadata->headers()->values('x-order'), bin2hex($metadata->headers()->values('x-binary-bin')[0])],
                    'busy' => errorClass(fn() => $dispatcher->tryReceive()),
                    'active' => $dispatcher->getInfo()->activeCount(),
                    'explicit_destruct' => $call->isFinalized(),
                    'clone' => errorClass(fn() => clone $call),
                    'serialize' => errorClass(fn() => serialize($metadata)),
                ];
                $cases = [
                    ['name' => 'reserved', 'method' => 'addHeader', 'key' => 'grpc-status', 'value' => '0'],
                    ['name' => 'binary_suffix', 'method' => 'addBinaryHeader', 'key' => 'x-token', 'value' => 'x'],
                    ['name' => 'text_suffix', 'method' => 'addHeader', 'key' => 'x-token-bin', 'value' => 'x'],
                    ['name' => 'control', 'method' => 'addHeader', 'key' => 'x-text', 'value' => "\t"],
                    ['name' => 'http_control', 'method' => 'addHeader', 'key' => 'Accept-Encoding', 'value' => 'gzip'],
                    ['name' => 'user_agent', 'method' => 'addHeader', 'key' => 'User-Agent', 'value' => 'agent'],
                ];
                foreach ($cases as $probe) {
                    $result[$probe['name']] = errorClass(fn() => $metadata->{$probe['method']}($probe['key'], $probe['value']));
                }
                $call->respond(json_encode($result, JSON_THROW_ON_ERROR));
                break;
            case 'fail':
                $metadata = $call->getResponseMetadata();
                $metadata->addHeader('x-error', 'header');
                $metadata->addBinaryTrailer('x-error-bin', "\0\xff");
                $call->fail(new Status(StatusCode::InvalidArgument, 'bad % input', [new ErrorDetail('type.googleapis.com/example.Detail', "\0\xff")]));
                break;
            case 'retained':
                $retained = $call->getResponseMetadata();
                $retained->addHeader('x-state', 'kept');
                $before = $retained->headers();
                $call->respond($call->getMessage());
                $report = [errorClass(fn() => $call->respond('again'))];
                unset($call);
                $report[] = errorClass(fn() => $retained->addTrailer('x-state', 'late'));
                $report[] = $before->values('x-state');
                $report[] = $retained->headers()->values('x-state');
                break;
            case 'abandoned':
                $retained = $call->getResponseMetadata();
                $retained->addHeader('x-state', 'kept');
                unset($call);
                $report = [errorClass(fn() => $retained->addHeader('x-state', 'late')), $retained->headers()->values('x-state')];
                break;
            case 'report':
                $report[] = $dispatcher->getInfo()->activeCount();
                $call->respond(json_encode($report, JSON_THROW_ON_ERROR));
                break;
            case 'throw':
                throw new RuntimeException('secret application error');
            case 'bailout':
                trigger_error('secret fatal error', E_USER_ERROR);
                break;
            case 'exit':
                exit;
            case 'busy':
                $cancelled = $call;
                $cancelledMetadata = $call->getResponseMetadata();
                Rapira\log('grpc-busy');
                usleep(400000);
                break;
            case 'cancel-report':
                $result = [$cancelled->isCancelled(), $cancelled->isFinalized(), errorClass(fn() => $cancelled->respond('late')), errorClass(fn() => $cancelledMetadata->addHeader('x-state', 'late')), $dispatcher->getInfo()->pendingCount(), $dispatcher->getInfo()->activeCount()];
                $call->respond(json_encode($result, JSON_THROW_ON_ERROR));
                break;
            default:
                $call->respond($call->getMessage());
        }
    }
} catch (Rapira\Exception\ClosedException) {
}
