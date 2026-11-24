//! The descriptor set: the method routes, the JSON transcoding and the service list of `getServices()`.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use buffa_descriptor::generated::descriptor::method_options::IdempotencyLevel;
use buffa_descriptor::{
    DescriptorPool, DynamicMessage, MessageIndex, ServiceDescriptor, ServiceIndex,
};
use connectrpc::MethodKind;
use ureq::tls::{RootCerts, TlsConfig};

/// Where `grpc.descriptor_set` points.
#[derive(Debug, PartialEq)]
pub enum Source {
    File(PathBuf),
    /// An `http://` or `https://` URL.
    Url(String),
}

/// The form for messages. A URL shows no userinfo and no query, because both can hold credentials.
impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::File(path) => path.display().fmt(f),
            Source::Url(url) => {
                let url = url.split(['?', '#']).next().unwrap_or_default();
                let (scheme, rest) = url.split_once("://").unwrap_or_default();
                // The userinfo ends at the last at sign of the authority.
                let authority = rest.split('/').next().unwrap_or_default();
                let host = authority.rfind('@').map_or(0, |at| at + 1);
                write!(f, "{scheme}://{}", &rest[host..])
            }
        }
    }
}

/// Gets the descriptor set at boot.
fn download(url: &str) -> Result<Vec<u8>, ureq::Error> {
    let timeout = Some(Duration::from_secs(30));
    let agent = ureq::Agent::config_builder()
        .timeout_connect(timeout)
        .timeout_recv_response(timeout)
        .timeout_recv_body(timeout)
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    agent
        .get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(64 << 20)
        .read_to_vec()
}

/// The services of a FileDescriptorSet that rapira serves.
pub struct Schema {
    pool: Arc<DescriptorPool>,
    /// The key is `package.Service/Method`, the request path without its leading slash.
    methods: HashMap<String, Method>,
    /// The configured services in configuration order.
    services: Vec<ServiceIndex>,
}

thread_local! {
    static SCHEMA: OnceCell<Arc<Schema>> = const { OnceCell::new() };
}

pub(crate) fn enter(schema: Arc<Schema>) {
    SCHEMA.with(|slot| assert!(slot.set(schema).is_ok()));
}

pub(crate) fn with<T>(f: impl FnOnce(&Schema) -> T) -> T {
    SCHEMA.with(|slot| f(slot.get().expect("the gRPC worker has its schema")))
}

/// A method that rapira routes to PHP: unary or server-streaming.
pub struct Method {
    pub input: MessageIndex,
    pub(crate) output: MessageIndex,
    /// The method has `idempotency_level = NO_SIDE_EFFECTS`, so Connect GET can call it.
    pub(crate) idempotent: bool,
    pub(crate) kind: MethodKind,
}

/// The services that the plugin answers itself. The plugin never routes them to PHP.
const PLUGIN_SERVICES: [&str; 3] = [
    connectrpc_health::HEALTH_SERVICE_NAME,
    connectrpc_reflection::SERVER_REFLECTION_SERVICE_NAME,
    connectrpc_reflection::SERVER_REFLECTION_V1ALPHA_SERVICE_NAME,
];

impl Schema {
    /// Loads the set at `source` and keeps the unary and server-streaming methods of `services` as routes. `None` serves the services of the files that no other file of the set imports.
    pub fn load(source: &Source, services: Option<&[String]>) -> anyhow::Result<Schema> {
        let bytes = match source {
            Source::File(path) => std::fs::read(path)
                .map_err(|e| anyhow!("reading grpc.descriptor_set {source}: {e}"))?,
            Source::Url(url) => download(url)
                .map_err(|e| anyhow!("downloading grpc.descriptor_set {source}: {e}"))?,
        };
        // The operator supplies the set, so the element memory limit for untrusted input does not apply.
        let opts = buffa::DecodeOptions::new().with_element_memory_limit(usize::MAX);
        let pool = DescriptorPool::decode_with_options(&bytes, &opts).map_err(|e| {
            anyhow!(
                "decoding grpc.descriptor_set {source}: {e}; build it with `buf build --as-file-descriptor-set` or `protoc --include_imports`"
            )
        })?;
        // The pool resolves types only, so a missing import that supplies only options still decodes. A reflection client needs every import. An edition 2024 import option goes to option_dependency and may be absent.
        for file in pool.files() {
            if let Some(import) = file
                .dependency
                .iter()
                .find(|name| pool.file_by_name(name).is_none())
            {
                return Err(anyhow!(
                    "grpc.descriptor_set {source}: {} imports {import}, which the set does not contain; build it with `buf build --as-file-descriptor-set` or `protoc --include_imports`",
                    file.name.as_deref().unwrap_or_default()
                ));
            }
        }

        let selected: Vec<&ServiceDescriptor> = match services {
            Some(names) => {
                let mut selected = Vec::with_capacity(names.len());
                for name in names {
                    if PLUGIN_SERVICES.contains(&name.trim_start_matches('.')) {
                        return Err(anyhow!(
                            "grpc.services entry `{name}` is served by the plugin"
                        ));
                    }
                    let service = pool.service_by_name(name).ok_or_else(|| {
                        anyhow!(
                            "grpc.services entry `{name}` is not in grpc.descriptor_set {source}"
                        )
                    })?;
                    if selected
                        .iter()
                        .any(|s: &&ServiceDescriptor| s.full_name() == service.full_name())
                    {
                        return Err(anyhow!(
                            "grpc.services lists `{}` twice",
                            service.full_name()
                        ));
                    }
                    selected.push(service);
                }
                selected
            }
            None => {
                // A file that another file imports is a dependency, such as google/longrunning/operations.proto.
                let mut imported = HashSet::new();
                for f in pool.files() {
                    for name in &f.dependency {
                        imported.insert(name.as_str());
                    }
                }
                let mut selected = Vec::new();
                for s in pool.services() {
                    let Some(file) = pool.file_containing_symbol(s.full_name()) else {
                        continue;
                    };
                    if imported.contains(file.name.as_deref().unwrap_or_default()) {
                        continue;
                    }
                    if PLUGIN_SERVICES.contains(&s.full_name()) {
                        tracing::debug!(target: "grpc", "{} is the plugin's own service; not routed to PHP", s.full_name());
                        continue;
                    }
                    selected.push(s);
                }
                if selected.is_empty() {
                    return Err(anyhow!(
                        "grpc.descriptor_set {source} declares no service to serve"
                    ));
                }
                selected
            }
        };

        let mut methods = HashMap::new();
        let mut listed = Vec::with_capacity(selected.len());
        for service in &selected {
            add_routes(service, &mut methods);
            let index = pool
                .service_index(service.full_name())
                .expect("the pool holds each selected service");
            listed.push(index);
        }

        Ok(Schema {
            pool: Arc::new(pool),
            methods,
            services: listed,
        })
    }

    /// The configured services in configuration order. A descriptor has every method of its service, streaming ones included, in descriptor order.
    pub(crate) fn services(&self) -> impl ExactSizeIterator<Item = &ServiceDescriptor> {
        self.services.iter().map(|&index| self.pool.service(index))
    }

    pub fn pool(&self) -> &Arc<DescriptorPool> {
        &self.pool
    }

    /// The route for `path` (`package.Service/Method`). Methods with a request stream and unlisted methods have none.
    pub fn method(&self, path: &str) -> Option<&Method> {
        self.methods.get(path)
    }

    /// Drops unknown fields. An unknown enum value name fails the decode.
    pub fn json_to_proto(&self, m: &Method, json: &[u8]) -> anyhow::Result<Vec<u8>> {
        let json = std::str::from_utf8(json)?;
        let msg =
            DynamicMessage::from_json_ignoring_unknown(Arc::clone(&self.pool), m.input, json)?;
        Ok(msg.encode_to_vec())
    }

    pub fn proto_to_json(&self, m: &Method, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        // The app produces the reply, so the element memory limit for untrusted input does not apply. to_json decodes an Any payload again under the default limit.
        let opts = buffa::DecodeOptions::new().with_element_memory_limit(usize::MAX);
        let msg =
            DynamicMessage::decode_with_options(Arc::clone(&self.pool), m.output, bytes, &opts)?;
        Ok(msg.to_json()?.into_bytes())
    }
}

/// Adds the unary and server-streaming methods of `service` to `methods` as routes.
fn add_routes(service: &ServiceDescriptor, methods: &mut HashMap<String, Method>) {
    for m in service.methods() {
        let route = format!("{}/{}", service.full_name(), m.name());
        if m.is_client_streaming() {
            tracing::warn!(target: "grpc", "{route} takes a stream of requests; rapira answers it with UNIMPLEMENTED");
        } else {
            let idempotent = m.options().and_then(|o| o.idempotency_level)
                == Some(IdempotencyLevel::NO_SIDE_EFFECTS);
            let kind = if m.is_server_streaming() {
                MethodKind::ServerStreaming
            } else {
                MethodKind::Unary
            };
            let method = Method {
                input: m.input(),
                output: m.output(),
                idempotent,
                kind,
            };
            methods.insert(route, method);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values: a message shows the scheme, the host, the port and the path of `scheme://userinfo@host:port/path?query#fragment`. The userinfo and the query can hold credentials. https://www.rfc-editor.org/rfc/rfc3986#section-3
    #[test]
    fn a_message_shows_a_url_without_its_credential_parts() {
        struct Case {
            name: &'static str,
            source: Source,
            expected: &'static str,
        }
        let url = |url: &str| Source::Url(url.to_owned());
        let cases = [
            Case {
                name: "file",
                source: Source::File("/etc/rapira/api.binpb".into()),
                expected: "/etc/rapira/api.binpb",
            },
            Case {
                name: "URL without credentials",
                source: url("https://example.com/sets/api.binpb"),
                expected: "https://example.com/sets/api.binpb",
            },
            Case {
                name: "userinfo",
                source: url("https://user:pass@example.com/api.binpb"),
                expected: "https://example.com/api.binpb",
            },
            Case {
                name: "query",
                source: url("https://example.com/api.binpb?sig=abc"),
                expected: "https://example.com/api.binpb",
            },
            Case {
                name: "fragment",
                source: url("https://example.com/api.binpb#abc"),
                expected: "https://example.com/api.binpb",
            },
            Case {
                name: "userinfo, port and query",
                source: url("http://user:pass@127.0.0.1:8080/api.binpb?sig=abc"),
                expected: "http://127.0.0.1:8080/api.binpb",
            },
            Case {
                name: "userinfo without a path",
                source: url("https://user:pass@example.com"),
                expected: "https://example.com",
            },
            Case {
                name: "at sign in the path",
                source: url("https://example.com/sets@v1/api.binpb"),
                expected: "https://example.com/sets@v1/api.binpb",
            },
            Case {
                name: "at sign in the query",
                source: url("https://example.com/api.binpb?user=a@b"),
                expected: "https://example.com/api.binpb",
            },
        ];
        for case in cases {
            assert_eq!(case.source.to_string(), case.expected, "{}", case.name);
        }
    }
}
