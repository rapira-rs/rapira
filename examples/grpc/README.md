# Unary gRPC example

This service returns the same protobuf message that it receives. Request and response use the same message type, so the PHP handler can return the raw bytes.

Install `grpcurl` to call the service through reflection. Rapira parses the included schema at startup.

Start Rapira from the repository root:

```sh
rapira serve examples/grpc/rapira.toml
```

List services:

```sh
grpcurl -plaintext 127.0.0.1:9001 list
```

Call the echo method:

```sh
grpcurl -plaintext -d '{"text":"hello"}' 127.0.0.1:9001 example.v1.Echo/Echo
```

The response is `{"text":"hello"}`. Add `-v` to show response headers and trailers. These client commands get their schemas from reflection.

Call the same method with binary Connect over HTTP/1.1:

```sh
printf '\012\005hello' | curl --silent --show-error --http1.1 \
  -H 'Content-Type: application/proto' \
  -H 'Connect-Protocol-Version: 1' \
  --data-binary @- http://127.0.0.1:9001/example.v1.Echo/Echo | od -An -tx1
```

The response bytes are `0a 05 68 65 6c 6c 6f`. Binary gRPC-Web clients can use the same method path with `application/grpc-web+proto` and gRPC message framing.

An application that changes message fields can generate PHP classes with `protoc --php_out` during its build. Decode `$call->getMessage()` with the generated request class. Pass the serialized response to `$call->respond()`.
