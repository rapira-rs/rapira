use connectrpc::{Limits, Router};
use connectrpc_reflection::{Reflector, apply_request_limits, install};

use crate::Config;

pub(crate) fn router(config: &Config, limits: Limits) -> anyhow::Result<Router> {
    let router = Router::new();
    if !config.reflection {
        return Ok(router);
    }
    let services = config
        .registry
        .services()
        .iter()
        .map(|service| service.name.as_str())
        .chain([
            "grpc.reflection.v1.ServerReflection",
            "grpc.reflection.v1alpha.ServerReflection",
        ]);
    let reflector = Reflector::from_descriptor_set_bytes(config.registry.encoded_descriptors())?
        .with_services(services);
    Ok(apply_request_limits(install(router, reflector), limits))
}
