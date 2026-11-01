//! The descriptor set: the method routes, the JSON transcoding and the service list of `getServices()`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::anyhow;
use buffa_descriptor::generated::descriptor::method_options::IdempotencyLevel;
use buffa_descriptor::{DescriptorPool, DynamicMessage, MessageIndex, ServiceDescriptor};

/// The services of a FileDescriptorSet that rapira serves.
pub struct Schema {
    pool: Arc<DescriptorPool>,
    /// Keyed `package.Service/Method`, the request path without its leading slash.
    methods: HashMap<String, Method>,
    services: Vec<ServiceInfo>,
}

/// A configured service with every method it declares.
pub(crate) struct ServiceInfo {
    pub name: String,
    pub methods: Vec<MethodInfo>,
}

/// A method with fully qualified message type names.
pub(crate) struct MethodInfo {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

/// The schema whose services `getServices()` reports. The master sets it in `prepare`, before the fork, so every worker inherits it.
static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();

/// The first call sets the schema; a later call changes nothing.
pub(crate) fn set_schema(schema: Arc<Schema>) {
    let _ = SCHEMA.set(schema);
}

/// The services that `getServices()` reports; empty before `set_schema`.
pub(crate) fn services() -> &'static [ServiceInfo] {
    SCHEMA.get().map_or(&[], |s| s.services())
}

/// A unary method that rapira routes to PHP.
pub struct Method {
    pub input: MessageIndex,
    pub(crate) output: MessageIndex,
    /// The method has `idempotency_level = NO_SIDE_EFFECTS`, so Connect GET can call it.
    pub(crate) idempotent: bool,
}

/// The services the plugin answers itself: never routed to PHP.
const PLUGIN_SERVICES: [&str; 3] = [
    connectrpc_health::HEALTH_SERVICE_NAME,
    connectrpc_reflection::SERVER_REFLECTION_SERVICE_NAME,
    connectrpc_reflection::SERVER_REFLECTION_V1ALPHA_SERVICE_NAME,
];

impl Schema {
    /// Loads the set at `path` and keeps the unary methods of `services` as routes. `None` serves the services of the files that no other file of the set imports.
    pub fn load(path: &Path, services: Option<&[String]>) -> anyhow::Result<Schema> {
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
        // The pool resolves types only, so a missing import that supplies only options still decodes. A reflection client needs every import. An edition 2024 import option goes to option_dependency and may be absent.
        for file in pool.files() {
            if let Some(import) = file
                .dependency
                .iter()
                .find(|name| pool.file_by_name(name).is_none())
            {
                return Err(anyhow!(
                    "grpc.descriptor_set {}: {} imports {import}, which the set does not contain; build it with `buf build --as-file-descriptor-set` or `protoc --include_imports`",
                    path.display(),
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
                            "grpc.services entry `{name}` is not in grpc.descriptor_set {}",
                            path.display()
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
                        "grpc.descriptor_set {} declares no service to serve",
                        path.display()
                    ));
                }
                selected
            }
        };

        let mut methods = HashMap::new();
        let mut listed = Vec::with_capacity(selected.len());
        for service in &selected {
            listed.push(service_info(&pool, service, &mut methods));
        }

        Ok(Schema {
            pool: Arc::new(pool),
            methods,
            services: listed,
        })
    }

    /// Every method of the configured services, streaming ones included, in descriptor order.
    pub(crate) fn services(&self) -> &[ServiceInfo] {
        &self.services
    }

    pub fn pool(&self) -> &Arc<DescriptorPool> {
        &self.pool
    }

    /// The route for `path` (`package.Service/Method`). Streaming and unlisted methods have none.
    pub fn method(&self, path: &str) -> Option<&Method> {
        self.methods.get(path)
    }

    /// Unknown fields are dropped. An unknown enum value name fails the decode.
    pub fn json_to_proto(&self, m: &Method, json: &[u8]) -> anyhow::Result<Vec<u8>> {
        let json = std::str::from_utf8(json)?;
        let msg =
            DynamicMessage::from_json_ignoring_unknown(Arc::clone(&self.pool), m.input, json)?;
        Ok(msg.encode_to_vec())
    }

    pub fn proto_to_json(&self, m: &Method, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        // The application produces the reply, so the element memory limit for untrusted input does not apply. to_json decodes an Any payload again under the default limit.
        let opts = buffa::DecodeOptions::new().with_element_memory_limit(usize::MAX);
        let msg =
            DynamicMessage::decode_with_options(Arc::clone(&self.pool), m.output, bytes, &opts)?;
        Ok(msg.to_json()?.into_bytes())
    }
}

/// The listing of `service`, with its unary methods added to `methods` as routes.
fn service_info(
    pool: &DescriptorPool,
    service: &ServiceDescriptor,
    methods: &mut HashMap<String, Method>,
) -> ServiceInfo {
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
    ServiceInfo {
        name: service.full_name().to_owned(),
        methods: infos,
    }
}
