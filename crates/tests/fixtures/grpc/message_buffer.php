<?php

function messageInfo(string $message): array {
    return [strlen($message), hash('sha256', $message)];
}

$shutdownCall = null;
$shutdownAction = null;
register_shutdown_function(function () use (&$shutdownCall, &$shutdownAction) {
    if ($shutdownCall === null) {
        return;
    }
    if ($shutdownAction === 'shutdown_allocate') {
        ini_set('memory_limit', (string)(memory_get_usage(true) + 1048576));
    }
    Rapira\log('grpc-message-shutdown', context: messageInfo($shutdownCall->getMessage()));
    if ($shutdownAction === 'shutdown_bailout') {
        trigger_error('message shutdown failure', E_USER_ERROR);
    }
});

$dispatcher = Rapira\get_dispatcher();
try {
    while ($call = $dispatcher->receive()) {
        $action = $call->getContext()->metadata->values('test-case')[0];
        switch ($action) {
            case 'retain_string':
                $retained = $call->getMessage();
                $result = messageInfo($retained);
                break;
            case 'check_string':
                $result = [messageInfo($call->getMessage()), messageInfo($retained)];
                unset($retained);
                break;
            case 'retain_call':
                $retainedCall = $call;
                $result = messageInfo($call->getMessage());
                break;
            case 'check_call':
                $result = [messageInfo($call->getMessage()), messageInfo($retainedCall->getMessage()), messageInfo($call->getMessage())];
                unset($retainedCall);
                break;
            case 'mutate':
                $payload = $call->getMessage();
                $payload[0] = '!';
                $payload[strlen($payload) - 1] = "\0";
                $result = [messageInfo($payload), messageInfo($call->getMessage())];
                unset($payload);
                break;
            case 'repeated':
                $first = $call->getMessage();
                $second = $call->getMessage();
                $first[0] = '!';
                $result = [messageInfo($first), messageInfo($second), messageInfo($call->getMessage())];
                unset($first, $second);
                break;
            case 'payload':
                $payload = $call->getMessage();
                $result = messageInfo($payload);
                break;
            case 'release_payload':
                unset($payload);
                $result = messageInfo($call->getMessage());
                break;
            case 'derived':
                $payload = $call->getMessage();
                $copy = substr($payload, 0, -1) . substr($payload, -1);
                $map = [$payload => 'found'];
                $result = [$map[$copy] ?? 'missing', preg_match('//u', $payload), preg_last_error()];
                unset($payload, $copy, $map);
                break;
            case 'throw':
                $payload = $call->getMessage();
                throw new RuntimeException('message failure');
            case 'bailout':
                $payload = $call->getMessage();
                trigger_error('message fatal failure', E_USER_ERROR);
                break;
            case 'exit':
                $payload = $call->getMessage();
                exit;
            case 'limit_shared':
                $payload = $call->getMessage();
                // The next getMessage() needs storage while $payload is live.
            case 'limit':
                ini_set('memory_limit', (string)(memory_get_usage(true) + 1048576));
                $result = messageInfo($call->getMessage());
                break;
            case 'shutdown':
            case 'shutdown_bailout':
            case 'shutdown_allocate':
                $shutdownCall = $call;
                $shutdownAction = $action;
                $payload = $call->getMessage();
                if ($action !== 'shutdown_allocate') {
                    $call->respond(json_encode(messageInfo($payload), JSON_THROW_ON_ERROR));
                    unset($payload);
                }
                return;
            default:
                $result = messageInfo($call->getMessage());
        }
        $call->respond(json_encode($result, JSON_THROW_ON_ERROR));
    }
} catch (Rapira\Exception\ClosedException) {
}
