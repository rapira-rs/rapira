<?php

use Rapira\Grpc\ErrorDetail;
use Rapira\Grpc\Exception\GrpcException;
use Rapira\Grpc\MethodInfo;
use Rapira\Grpc\MethodKind;
use Rapira\Grpc\ServiceInfo;
use Rapira\Grpc\Status;
use Rapira\Grpc\StatusCode;

$cases = [
    [
        'name' => 'rich_status',
        'run' => static function (): array {
            $status = new Status(StatusCode::InvalidArgument, 'bad % input', [
                new ErrorDetail('type.googleapis.com/example.Detail', "\x00\xff"),
            ]);
            return [$status->code->value, $status->message, $status->details[0]->typeUrl, bin2hex($status->details[0]->value)];
        },
    ],
    [
        'name' => 'invalid_detail_type',
        'run' => static fn () => new Status(StatusCode::InvalidArgument, '', [new stdClass()]),
    ],
    [
        'name' => 'invalid_method_type',
        'run' => static fn () => new ServiceInfo('example.Echo', [new stdClass()]),
    ],
    [
        'name' => 'exception_status',
        'run' => static function (): array {
            $error = new GrpcException(StatusCode::Unavailable, 'busy', [
                new ErrorDetail('type.googleapis.com/example.Detail', "\x08\x01"),
            ]);
            return [$error->getMessage(), $error->status->code->value, bin2hex($error->status->details[0]->value)];
        },
    ],
    [
        'name' => 'unary_axes',
        'run' => static fn () => [MethodKind::Unary->isStreamingRequest(), MethodKind::Unary->isStreamingResponse()],
    ],
    [
        'name' => 'exception_constructor_message_precedence',
        'run' => static function (): array {
            $error = new class(StatusCode::Unavailable) extends GrpcException {
                protected $message = 'fallback';
            };
            return [$error->getMessage(), $error->status->message];
        },
    ],
    [
        'name' => 'client_streaming_axes',
        'run' => static fn () => [MethodKind::ClientStreaming->isStreamingRequest(), MethodKind::ClientStreaming->isStreamingResponse()],
    ],
    [
        'name' => 'server_streaming_axes',
        'run' => static fn () => [MethodKind::ServerStreaming->isStreamingRequest(), MethodKind::ServerStreaming->isStreamingResponse()],
    ],
    [
        'name' => 'bidi_streaming_axes',
        'run' => static fn () => [MethodKind::BidiStreaming->isStreamingRequest(), MethodKind::BidiStreaming->isStreamingResponse()],
    ],
    [
        'name' => 'service_methods_snapshot',
        'run' => static function (): array {
            $method = new MethodInfo('Echo', 'example.Message', 'example.Message', MethodKind::Unary);
            $methods = [&$method];
            $service = new ServiceInfo('example.Echo', $methods);
            $method = new MethodInfo('Other', 'other.Message', 'other.Message', MethodKind::Unary);
            return [$service->methods[0]->name, $service->methods[0]->inputType];
        },
    ],
];

while (\Rapira\handle_request(static function () use ($cases): void {
    $results = [];
    foreach ($cases as $case) {
        try {
            $results[$case['name']] = ($case['run'])();
        } catch (Throwable $error) {
            $results[$case['name']] = $error::class;
        }
    }
    echo json_encode($results, JSON_THROW_ON_ERROR);
})) {
}
