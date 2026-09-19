use bytes::Bytes;
use extension_api::{Extension, Php, grpc};
use php_sys::{Mode, Rapira};
use rapira_runtime::{ExtensionRuntime, grpc_services};
use serde_json::json;
use std::{sync::mpsc, time::Duration};
use tests::{fixture, php_lock};

struct Probe(mpsc::SyncSender<anyhow::Result<grpc::Reply>>);

impl Extension for Probe {
    type Config = mpsc::SyncSender<anyhow::Result<grpc::Reply>>;
    fn init(config: Self::Config) -> Self {
        Self(config)
    }
    fn name(&self) -> &str {
        "grpc-php-probe"
    }
    async fn run(&mut self, php: Php) -> anyhow::Result<()> {
        let result = php
            .exec_grpc(grpc::Request {
                method: "example.Echo/Probe".into(),
                message: Bytes::from_static(b"\0\xff"),
                metadata: vec![("x-bytes-bin".into(), vec![0, 255])],
                remote: extension_api::Addr::Unix(Some("/run/client.sock".into())),
                tls: Some(extension_api::Tls {
                    version: "TLSv1.3".into(),
                    cipher: "TLS_AES_128_GCM_SHA256".into(),
                    alpn: Some("h2".into()),
                    server_name: Some("rpc.example".into()),
                    cert: Some(extension_api::ClientCert {
                        serial: "42".into(),
                        organization: Some("Example".into()),
                        fingerprint: "abc".into(),
                    }),
                }),
                received_at: 1234.25,
                deadline: Some(5678.5),
                expires_at: None,
            })
            .await;
        self.0.send(result).unwrap();
        Ok(())
    }
    async fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn runtime_maps_unary_services_context_and_binary_reply() -> anyhow::Result<()> {
    let _guard = php_lock();
    let services = vec![grpc::ServiceInfo {
        name: "example.Echo".into(),
        methods: vec![grpc::MethodInfo {
            name: "Probe".into(),
            input_type: "example.Input".into(),
            output_type: "example.Output".into(),
        }],
    }];
    let path = fixture("grpc/runtime.php");
    let rapira = Rapira::start(Mode::GrpcDispatcher {
        entrypoint: path.clone(),
        services: grpc_services(&services),
    })?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let mut extensions = ExtensionRuntime::new();
    extensions.register::<Probe>(sender)?;
    let running = extensions.run(rapira.handle(), path);
    let reply = receiver.recv_timeout(Duration::from_secs(10))??;
    let outcomes = running.join();
    rapira.shutdown();
    assert!(outcomes.into_iter().all(|result| result.is_ok()));
    assert_eq!(reply.trailers, vec![("x-reply-bin".into(), vec![0, 255])]);
    let actual: serde_json::Value = serde_json::from_slice(&reply.result.unwrap())?;
    assert_eq!(
        actual,
        json!([
            [["Probe", "unary"]],
            ["example.Echo/Probe", "/run/client.sock", 5678.5, 1234.25],
            [
                "Rapira\\Tls",
                "TLSv1.3",
                "TLS_AES_128_GCM_SHA256",
                "h2",
                "rpc.example",
                "42",
                "Example",
                "abc"
            ],
            "00ff"
        ])
    );
    Ok(())
}
