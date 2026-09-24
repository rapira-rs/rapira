use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::anyhow;
use buffa_descriptor::generated::descriptor::method_options::IdempotencyLevel;
use buffa_descriptor::{DescriptorPool, DynamicMessage, MessageIndex};

/// The services of a FileDescriptorSet that rapira serves.
pub struct Schema {
    pool: Arc<DescriptorPool>,
    /// Keyed `package.Service/Method`, the request path without its leading slash.
    methods: HashMap<String, Method>,
    services: Vec<ServiceInfo>,
}

/// A configured service with every method it declares.
#[derive(Debug, PartialEq, Eq)]
pub struct ServiceInfo {
    pub name: String,
    pub methods: Vec<MethodInfo>,
}

/// A method with fully qualified message type names.
#[derive(Debug, PartialEq, Eq)]
pub struct MethodInfo {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

/// A unary method that rapira routes to PHP.
pub(crate) struct Method {
    pub(crate) input: MessageIndex,
    pub(crate) output: MessageIndex,
    /// The method has `idempotency_level = NO_SIDE_EFFECTS`, so Connect GET can call it.
    pub(crate) idempotent: bool,
}

impl Schema {
    /// Loads the set at `path` and keeps the unary methods of `services` as routes.
    pub fn load(path: &Path, services: &[String]) -> anyhow::Result<Schema> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow!("reading grpc.descriptor_set {}: {e}", path.display()))?;
        // The operator supplies the set, so the element memory limit for untrusted input does not apply.
        let opts = buffa::DecodeOptions::new().with_element_memory_limit(usize::MAX);
        let pool = DescriptorPool::decode_with_options(&bytes, &opts).map_err(|e| {
            anyhow!(
                "decoding grpc.descriptor_set {}: {e}; build it with `buf build --as-file-descriptor-set` or `protoc --include_imports`",
                path.display()
            )
        })?;

        let mut methods = HashMap::new();
        let mut listed = Vec::with_capacity(services.len());
        for name in services {
            let service = pool.service_by_name(name).ok_or_else(|| {
                anyhow!(
                    "grpc.services entry `{name}` is not in grpc.descriptor_set {}",
                    path.display()
                )
            })?;
            let full_name = service.full_name();
            if listed.iter().any(|s: &ServiceInfo| s.name == full_name) {
                return Err(anyhow!("grpc.services lists `{full_name}` twice"));
            }
            let mut infos = Vec::with_capacity(service.methods().len());
            for m in service.methods() {
                let route = format!("{}/{}", service.full_name(), m.name());
                if m.is_client_streaming() || m.is_server_streaming() {
                    tracing::warn!(target: "grpc", "{route} streams; rapira answers it with UNIMPLEMENTED");
                } else {
                    let idempotent = m.options().and_then(|o| o.idempotency_level)
                        == Some(IdempotencyLevel::NO_SIDE_EFFECTS);
                    let method = Method {
                        input: m.input(),
                        output: m.output(),
                        idempotent,
                    };
                    methods.insert(route, method);
                }
                infos.push(MethodInfo {
                    name: m.name().to_owned(),
                    input_type: pool.message(m.input()).full_name().to_owned(),
                    output_type: pool.message(m.output()).full_name().to_owned(),
                    client_streaming: m.is_client_streaming(),
                    server_streaming: m.is_server_streaming(),
                });
            }
            listed.push(ServiceInfo {
                name: service.full_name().to_owned(),
                methods: infos,
            });
        }

        Ok(Schema {
            pool: Arc::new(pool),
            methods,
            services: listed,
        })
    }

    /// Every method of the configured services, streaming ones included, in descriptor order.
    pub fn services(&self) -> &[ServiceInfo] {
        &self.services
    }

    pub(crate) fn pool(&self) -> &Arc<DescriptorPool> {
        &self.pool
    }

    /// The route for `path` (`package.Service/Method`). Streaming and unlisted methods have none.
    pub(crate) fn method(&self, path: &str) -> Option<&Method> {
        self.methods.get(path)
    }

    /// Unknown fields are dropped. An unknown enum value name fails the decode.
    pub(crate) fn json_to_proto(&self, m: &Method, json: &[u8]) -> anyhow::Result<Vec<u8>> {
        let json = std::str::from_utf8(json)?;
        let msg =
            DynamicMessage::from_json_ignoring_unknown(Arc::clone(&self.pool), m.input, json)?;
        Ok(msg.encode_to_vec())
    }

    pub(crate) fn proto_to_json(&self, m: &Method, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        // The application produces the reply, so the element memory limit for untrusted input does not apply.
        let opts = buffa::DecodeOptions::new().with_element_memory_limit(usize::MAX);
        let msg =
            DynamicMessage::decode_with_options(Arc::clone(&self.pool), m.output, bytes, &opts)?;
        Ok(msg.to_json()?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const ECHO: &str = "rapira.test.v1.EchoService";

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    fn echo() -> Schema {
        Schema::load(&testdata("echo.binpb"), &[ECHO.to_owned()]).expect("echo.binpb loads")
    }

    /// Streaming methods and services outside grpc.services get no route.
    #[test]
    fn load_routes_only_configured_unary_methods() {
        struct Case {
            name: &'static str,
            path: &'static str,
            idempotent: Option<bool>,
        }
        let cases = [
            Case {
                name: "unary method",
                path: "rapira.test.v1.EchoService/Echo",
                idempotent: Some(false),
            },
            Case {
                name: "no-side-effects method",
                path: "rapira.test.v1.EchoService/Get",
                idempotent: Some(true),
            },
            Case {
                name: "streaming method",
                path: "rapira.test.v1.EchoService/Watch",
                idempotent: None,
            },
            Case {
                name: "unlisted service",
                path: "rapira.test.v1.OtherService/Ping",
                idempotent: None,
            },
            Case {
                name: "unknown path",
                path: "x.Y/Z",
                idempotent: None,
            },
        ];
        let schema = echo();
        for case in cases {
            assert_eq!(
                schema.method(case.path).map(|m| m.idempotent),
                case.idempotent,
                "{}",
                case.name
            );
        }
    }

    /// The listing keeps streaming methods, in echo.proto order.
    #[test]
    fn services_report_every_configured_method() {
        let method = |name: &str, server_streaming| MethodInfo {
            name: name.to_owned(),
            input_type: "rapira.test.v1.EchoRequest".to_owned(),
            output_type: "rapira.test.v1.EchoResponse".to_owned(),
            client_streaming: false,
            server_streaming,
        };
        let want = [ServiceInfo {
            name: ECHO.to_owned(),
            methods: vec![
                method("Echo", false),
                method("Get", false),
                method("Watch", true),
            ],
        }];
        assert_eq!(echo().services(), want);
    }

    #[test]
    fn load_rejects_a_set_it_cannot_serve() {
        struct Case {
            name: &'static str,
            path: PathBuf,
            service: &'static str,
            error: &'static str,
        }
        let dir = tempfile::tempdir().unwrap();
        let not_a_set = dir.path().join("not-a-set.binpb");
        std::fs::write(&not_a_set, [0xff, 0xff]).unwrap();
        let cases = [
            Case {
                name: "unknown service",
                path: testdata("echo.binpb"),
                service: "rapira.test.v1.Missing",
                error: "grpc.services entry `rapira.test.v1.Missing` is not in",
            },
            Case {
                name: "missing file",
                path: PathBuf::from("/nonexistent.binpb"),
                service: ECHO,
                error: "reading grpc.descriptor_set",
            },
            Case {
                name: "not a set",
                path: not_a_set,
                service: ECHO,
                error: "--include_imports",
            },
            Case {
                name: "set without its imports",
                path: testdata("echo-no-imports.binpb"),
                service: ECHO,
                error: "--include_imports",
            },
        ];
        for case in cases {
            let Err(err) = Schema::load(&case.path, &[case.service.to_owned()]) else {
                panic!("{}: the set loaded", case.name);
            };
            let err = err.to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }

    /// buffa resolves a name with a leading dot to the same service.
    #[test]
    fn load_rejects_a_service_listed_twice() {
        struct Case {
            name: &'static str,
            services: [&'static str; 2],
        }
        let cases = [
            Case {
                name: "exact duplicate",
                services: [ECHO, ECHO],
            },
            Case {
                name: "leading-dot alias",
                services: [ECHO, ".rapira.test.v1.EchoService"],
            },
        ];
        for case in cases {
            let services = case.services.map(str::to_owned);
            let Err(err) = Schema::load(&testdata("echo.binpb"), &services) else {
                panic!("{}: the set loaded", case.name);
            };
            let err = err.to_string();
            assert!(
                err.contains("grpc.services lists `rapira.test.v1.EchoService` twice"),
                "{}: {err}",
                case.name
            );
        }
    }

    /// Expected bytes follow the protobuf encoding spec: field 1 string "hi"
    /// is 0a 02 68 69, field 2 Timestamp{seconds: 1} is 12 02 08 01.
    /// Field 3 packed repeated int32 is 1a, the varint length, then one varint per element.
    /// The proto3 JSON mapping writes a repeated int32 as an array of numbers.
    #[test]
    fn json_transcodes_by_descriptor() {
        // buffa charges the size of its Value type, at least 64 bytes, per element against a 32 MiB budget, so at most 524,288 elements fit.
        const IDS: usize = 600_000;
        let mut many_ids = vec![0x1a, 0xc0, 0xcf, 0x24];
        many_ids.resize(many_ids.len() + IDS, 0x01);
        let many_ids_json = format!(r#"{{"ids":[{}1]}}"#, "1,".repeat(IDS - 1));
        enum Way {
            ToProto,
            ToJson,
        }
        struct Case<'a> {
            name: &'static str,
            way: Way,
            input: &'a [u8],
            output: Option<&'a [u8]>,
        }
        let cases = [
            Case {
                name: "json to proto",
                way: Way::ToProto,
                input: br#"{"text":"hi"}"#,
                output: Some(&[0x0a, 0x02, 0x68, 0x69]),
            },
            Case {
                name: "well-known type from json",
                way: Way::ToProto,
                input: br#"{"text":"hi","at":"1970-01-01T00:00:01Z"}"#,
                output: Some(&[0x0a, 0x02, 0x68, 0x69, 0x12, 0x02, 0x08, 0x01]),
            },
            Case {
                name: "unknown field ignored",
                way: Way::ToProto,
                input: br#"{"text":"hi","x":1}"#,
                output: Some(&[0x0a, 0x02, 0x68, 0x69]),
            },
            Case {
                name: "wrong json type",
                way: Way::ToProto,
                input: br#"{"text":1}"#,
                output: None,
            },
            Case {
                name: "not json",
                way: Way::ToProto,
                input: b"not json",
                output: None,
            },
            Case {
                name: "invalid utf-8",
                way: Way::ToProto,
                input: b"{\"text\":\"\xff\"}",
                output: None,
            },
            Case {
                name: "proto to json",
                way: Way::ToJson,
                input: &[0x0a, 0x02, 0x68, 0x69],
                output: Some(br#"{"text":"hi"}"#),
            },
            Case {
                name: "defaults omitted",
                way: Way::ToJson,
                input: &[],
                output: Some(b"{}"),
            },
            Case {
                name: "well-known type to json",
                way: Way::ToJson,
                input: &[0x0a, 0x02, 0x68, 0x69, 0x12, 0x02, 0x08, 0x01],
                output: Some(br#"{"text":"hi","at":"1970-01-01T00:00:01Z"}"#),
            },
            Case {
                name: "undecodable proto",
                way: Way::ToJson,
                input: &[0xff],
                output: None,
            },
            Case {
                name: "reply above the untrusted-input element budget",
                way: Way::ToJson,
                input: &many_ids,
                output: Some(many_ids_json.as_bytes()),
            },
        ];
        let schema = echo();
        let m = schema.method("rapira.test.v1.EchoService/Echo").unwrap();
        for case in cases {
            let got = match case.way {
                Way::ToProto => schema.json_to_proto(m, case.input),
                Way::ToJson => schema.proto_to_json(m, case.input),
            };
            assert_eq!(got.as_deref().ok(), case.output, "{}: {got:?}", case.name);
        }
    }
}
