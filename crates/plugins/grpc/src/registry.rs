use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, ensure};
use extension_api::grpc::{MethodInfo, ServiceInfo};
use prost::Message;
use prost_types::FileDescriptorSet;

use crate::schema;

#[derive(Debug)]
pub struct Registry {
    services: Vec<ServiceInfo>,
    methods: BTreeSet<String>,
    encoded_descriptors: Vec<u8>,
}

impl Registry {
    /// The compiler searches `protos` before `import_paths`, in configuration order within each group.
    ///
    /// Equivalent roots share one search entry. Nested roots keep their import namespaces.
    ///
    /// Each import name must refer to one file across all configured roots. Discovered file aliases produce an error.
    ///
    /// Import paths: <https://protobuf.dev/programming-guides/proto3/#importing>.
    pub fn load(protos: &[PathBuf], import_paths: &[PathBuf]) -> anyhow::Result<Self> {
        let schema = schema::compile(protos, import_paths)?;
        let descriptors = FileDescriptorSet::decode(schema.encoded_descriptors.as_slice())
            .context("decoding protobuf descriptors")?;
        let mut services = Vec::new();
        for file in &descriptors.file {
            for service in &file.service {
                let name = if file.package().is_empty() {
                    service.name().to_owned()
                } else {
                    format!("{}.{}", file.package(), service.name())
                };
                ensure!(
                    !matches!(
                        name.as_str(),
                        "grpc.reflection.v1.ServerReflection"
                            | "grpc.reflection.v1alpha.ServerReflection"
                    ),
                    "reserved reflection service {name} in {}",
                    file.name()
                );
                if !schema.entry_names.contains(file.name()) {
                    continue;
                }
                let mut methods = Vec::new();
                for method in &service.method {
                    ensure!(
                        !method.client_streaming() && !method.server_streaming(),
                        "streaming method /{name}/{} in {} is not supported",
                        method.name(),
                        file.name()
                    );
                    methods.push(MethodInfo {
                        name: method.name().to_owned(),
                        input_type: method.input_type().trim_start_matches('.').to_owned(),
                        output_type: method.output_type().trim_start_matches('.').to_owned(),
                    });
                }
                services.push(ServiceInfo { name, methods });
            }
        }
        services.sort_by(|left, right| left.name.cmp(&right.name));
        let mut methods = BTreeSet::new();
        for service in &services {
            for method in &service.methods {
                methods.insert(format!("/{}/{}", service.name, method.name));
            }
        }
        Ok(Self {
            services,
            methods,
            encoded_descriptors: schema.encoded_descriptors,
        })
    }

    pub fn services(&self) -> &[ServiceInfo] {
        &self.services
    }

    pub fn encoded_descriptors(&self) -> &[u8] {
        &self.encoded_descriptors
    }

    pub fn has_method(&self, path: &str) -> bool {
        self.methods.contains(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use prost_types::FileDescriptorSet;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    const APP: &str = r#"
        syntax = "proto3";
        package example;
        import "dependency.proto";
        service Zeta {
            rpc Second(vendor.Input) returns (vendor.Output);
            rpc First(vendor.Input) returns (vendor.Output);
        }
        service Alpha {
            rpc Call(vendor.Input) returns (vendor.Output);
        }
    "#;
    const DEPENDENCY: &str = r#"
        syntax = "proto3";
        package vendor;
        message Input { bytes payload = 1; }
        message Output {}
        service Imported {
            rpc Stream(stream Input) returns (stream Output);
        }
    "#;
    const OTHER: &str = r#"
        syntax = "proto3";
        package zulu;
        message Empty {}
        service Other { rpc Call(Empty) returns (Empty); }
    "#;
    const UNARY: &str = r#"
        syntax = "proto3";
        message Empty {}
        service Test { rpc Call(Empty) returns (Empty); }
    "#;

    fn write_files(root: &Path, files: &[(&str, &str)]) {
        for (name, contents) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    fn service_file<'a>(descriptors: &'a FileDescriptorSet, name: &str) -> &'a str {
        let (package, name) = name.rsplit_once('.').unwrap_or(("", name));
        descriptors
            .file
            .iter()
            .find(|file| {
                file.package() == package
                    && file.service.iter().any(|service| service.name() == name)
            })
            .unwrap()
            .name()
    }

    #[test]
    fn nested_import_roots_keep_each_namespace() {
        struct Case {
            name: &'static str,
            protos: &'static [&'static str],
            imports: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "parent_before_nested_vendor_root",
                protos: &["proto/app"],
                imports: &["proto", "proto/vendor/googleapis"],
            },
            Case {
                name: "nested_vendor_root_before_parent",
                protos: &["proto/app"],
                imports: &["proto/vendor/googleapis", "proto"],
            },
            Case {
                name: "equivalent_roots_keep_the_first_position",
                protos: &["proto/app", "proto/app/./"],
                imports: &["proto", "proto/app", "proto/vendor/googleapis", "proto/./"],
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(
                dir.path(),
                &[
                    (
                        "proto/app/service.proto",
                        r#"
                    syntax = "proto3";
                    package app;
                    import "messages.proto";
                    import "google/api/annotations.proto";
                    import "common/types.proto";
                    service Echo {
                        rpc Call(Input) returns (common.Output) {
                            option (google.api.http) = { post: "/v1/echo" body: "*" };
                        }
                    }
                "#,
                    ),
                    (
                        "proto/app/messages.proto",
                        "syntax = \"proto3\"; package app; message Input {}",
                    ),
                    (
                        "proto/common/types.proto",
                        "syntax = \"proto3\"; package common; message Output {}",
                    ),
                    (
                        "proto/vendor/googleapis/google/api/annotations.proto",
                        r#"
                    syntax = "proto3";
                    package google.api;
                    import "google/protobuf/descriptor.proto";
                    message HttpRule { string post = 1; string body = 2; }
                    extend google.protobuf.MethodOptions { HttpRule http = 72295728; }
                "#,
                    ),
                ],
            );
            let protos = case
                .protos
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let imports = case
                .imports
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let registry = Registry::load(&protos, &imports).expect(case.name);
            assert!(registry.has_method("/app.Echo/Call"), "{}", case.name);
            assert_eq!(registry.services().len(), 1, "{}", case.name);
            let descriptors = FileDescriptorSet::decode(registry.encoded_descriptors()).unwrap();
            assert_eq!(
                service_file(&descriptors, "app.Echo"),
                "service.proto",
                "{}",
                case.name
            );
            let mut names = descriptors
                .file
                .iter()
                .map(|file| file.name())
                .collect::<Vec<_>>();
            names.sort();
            assert_eq!(
                names,
                [
                    "common/types.proto",
                    "google/api/annotations.proto",
                    "google/protobuf/descriptor.proto",
                    "messages.proto",
                    "service.proto"
                ],
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn rejects_discovered_symlink_file_aliases() {
        struct Case {
            name: &'static str,
            import: &'static str,
            target: &'static str,
            link: &'static str,
            alias_file: &'static str,
        }
        let cases = [
            Case {
                name: "imported_file_alias",
                import: "types.proto",
                target: "proto/internal/types.proto",
                link: "proto/types.proto",
                alias_file: "proto/types.proto",
            },
            Case {
                name: "unreferenced_file_alias",
                import: "internal/types.proto",
                target: "proto/internal/types.proto",
                link: "proto/types.proto",
                alias_file: "proto/types.proto",
            },
            Case {
                name: "file_alias_through_directory_symlink",
                import: "aliases/types.proto",
                target: "proto/internal",
                link: "proto/aliases",
                alias_file: "proto/aliases/types.proto",
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(
                dir.path(),
                &[
                    (
                        "proto/service.proto",
                        &format!(
                            r#"
                    syntax = "proto3";
                    import "{}";
                    service Test {{ rpc Call(vendor.Input) returns (vendor.Output); }}
                "#,
                            case.import
                        ),
                    ),
                    (
                        "proto/internal/types.proto",
                        "syntax = \"proto3\"; package vendor; message Input {} message Output {}",
                    ),
                ],
            );
            symlink(dir.path().join(case.target), dir.path().join(case.link)).unwrap();
            let error = Registry::load(&[dir.path().join("proto")], &[]).expect_err(case.name);
            let text = format!("{error:#}");
            assert!(
                text.contains("symlink proto file alias"),
                "{}: {text}",
                case.name
            );
            let root = dir.path().canonicalize().unwrap();
            assert!(
                text.contains(&root.join(case.alias_file).display().to_string()),
                "{}: {text}",
                case.name
            );
            assert!(
                text.contains(
                    &root
                        .join("proto/internal/types.proto")
                        .display()
                        .to_string()
                ),
                "{}: {text}",
                case.name
            );
        }
    }

    #[test]
    fn method_types_use_contract_names() {
        struct Case {
            name: &'static str,
            schema: &'static str,
            input: &'static str,
            output: &'static str,
        }
        let cases = [
            Case {
                name: "package_qualified_messages",
                schema: "syntax = \"proto3\"; package vendor; message Input {} message Output {} service Test { rpc Call(Input) returns (Output); }",
                input: "vendor.Input",
                output: "vendor.Output",
            },
            Case {
                name: "unqualified_messages",
                schema: "syntax = \"proto3\"; message Input {} message Output {} service Test { rpc Call(Input) returns (Output); }",
                input: "Input",
                output: "Output",
            },
            Case {
                name: "nested_message_names",
                schema: "syntax = \"proto3\"; package vendor; message Envelope { message Input {} message Output {} } service Test { rpc Call(Envelope.Input) returns (Envelope.Output); }",
                input: "vendor.Envelope.Input",
                output: "vendor.Envelope.Output",
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(dir.path(), &[("schema/service.proto", case.schema)]);
            let registry = Registry::load(&[dir.path().join("schema")], &[]).expect(case.name);
            let method = &registry.services()[0].methods[0];
            assert_eq!(method.input_type, case.input, "{}", case.name);
            assert_eq!(method.output_type, case.output, "{}", case.name);
        }
    }

    #[test]
    fn discovers_entries_and_filters_imported_services() {
        struct Case {
            name: &'static str,
            protos: &'static [&'static str],
            imports: &'static [&'static str],
            links: &'static [(&'static str, &'static str)],
            entry_file: &'static str,
            descriptors: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "nested_entries_and_external_import",
                protos: &["schema"],
                imports: &["include"],
                links: &[],
                entry_file: "nested/app.proto",
                descriptors: &["dependency.proto", "nested/app.proto", "other.proto"],
            },
            Case {
                name: "overlapping_roots",
                protos: &["schema/nested", "schema", "schema/./"],
                imports: &["include", "include/./"],
                links: &[],
                entry_file: "app.proto",
                descriptors: &["app.proto", "dependency.proto", "other.proto"],
            },
            Case {
                name: "reversed_overlapping_roots",
                protos: &["schema", "schema/nested"],
                imports: &["include"],
                links: &[],
                entry_file: "nested/app.proto",
                descriptors: &["dependency.proto", "nested/app.proto", "other.proto"],
            },
            Case {
                name: "symlink_root_and_recursion_cycle",
                protos: &["alias", "schema"],
                imports: &["include"],
                links: &[("schema", "alias"), ("schema", "schema/nested/back")],
                entry_file: "nested/app.proto",
                descriptors: &["dependency.proto", "nested/app.proto", "other.proto"],
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(
                dir.path(),
                &[
                    ("schema/nested/app.proto", APP),
                    ("schema/other.proto", OTHER),
                    ("schema/ignored.txt", "not a schema"),
                    ("include/dependency.proto", DEPENDENCY),
                    ("include/unused.proto", UNARY),
                ],
            );
            for (target, link) in case.links {
                fs::create_dir_all(dir.path().join(link).parent().unwrap()).unwrap();
                symlink(dir.path().join(target), dir.path().join(link)).unwrap();
            }
            let protos = case
                .protos
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let imports = case
                .imports
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let registry = Registry::load(&protos, &imports).expect(case.name);
            assert_eq!(
                registry
                    .services()
                    .iter()
                    .map(|service| service.name.as_str())
                    .collect::<Vec<_>>(),
                ["example.Alpha", "example.Zeta", "zulu.Other"],
                "{}",
                case.name
            );
            assert_eq!(
                registry.services()[1]
                    .methods
                    .iter()
                    .map(|method| method.name.as_str())
                    .collect::<Vec<_>>(),
                ["Second", "First"],
                "{}",
                case.name
            );
            assert!(registry.has_method("/example.Zeta/Second"), "{}", case.name);
            let method = &registry.services()[1].methods[0];
            assert_eq!(method.input_type, "vendor.Input", "{}", case.name);
            assert_eq!(method.output_type, "vendor.Output", "{}", case.name);
            assert!(
                !registry.has_method("/vendor.Imported/Stream"),
                "{}",
                case.name
            );
            assert!(
                !registry.has_method("/example.Zeta/Missing"),
                "{}",
                case.name
            );
            assert!(!registry.has_method("example.Zeta/Second"), "{}", case.name);
            let descriptors = FileDescriptorSet::decode(registry.encoded_descriptors()).unwrap();
            assert_eq!(
                service_file(&descriptors, "example.Zeta"),
                case.entry_file,
                "{}",
                case.name
            );
            let mut names = descriptors
                .file
                .iter()
                .map(|file| file.name())
                .collect::<Vec<_>>();
            names.sort();
            assert_eq!(names, case.descriptors, "{}", case.name);
        }
    }

    #[test]
    fn parent_import_roots_keep_application_entries_separate() {
        struct Case {
            name: &'static str,
            imports: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "ancestor_import_root",
                imports: &["schema"],
            },
            Case {
                name: "overlapping_import_roots",
                imports: &["schema/vendor", "schema/app", "schema"],
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(
                dir.path(),
                &[
                    (
                        "schema/app/service.proto",
                        r#"
                    syntax = "proto3";
                    import "vendor/dependency.proto";
                    service Test { rpc Call(vendor.Input) returns (vendor.Output); }
                "#,
                    ),
                    ("schema/vendor/dependency.proto", DEPENDENCY),
                    ("schema/unused.proto", OTHER),
                ],
            );
            let imports = case
                .imports
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let registry =
                Registry::load(&[dir.path().join("schema/app")], &imports).expect(case.name);
            assert_eq!(registry.services().len(), 1, "{}", case.name);
            assert!(registry.has_method("/Test/Call"), "{}", case.name);
            let method = &registry.services()[0].methods[0];
            assert_eq!(method.input_type, "vendor.Input", "{}", case.name);
            assert!(
                !registry.has_method("/vendor.Imported/Stream"),
                "{}",
                case.name
            );
            let descriptors = FileDescriptorSet::decode(registry.encoded_descriptors()).unwrap();
            assert_eq!(
                service_file(&descriptors, "Test"),
                "service.proto",
                "{}",
                case.name
            );
            let mut names = descriptors
                .file
                .iter()
                .map(|file| file.name())
                .collect::<Vec<_>>();
            names.sort();
            assert_eq!(
                names,
                ["service.proto", "vendor/dependency.proto"],
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn validates_discovery_and_application_schemas() {
        struct Case {
            name: &'static str,
            files: &'static [(&'static str, &'static str)],
            directories: &'static [&'static str],
            protos: &'static [&'static str],
            imports: &'static [&'static str],
            error: &'static str,
        }
        let cases = [
            Case {
                name: "no_roots",
                files: &[],
                directories: &[],
                protos: &[],
                imports: &[],
                error: "at least one proto directory",
            },
            Case {
                name: "missing_root",
                files: &[],
                directories: &[],
                protos: &["missing"],
                imports: &[],
                error: "proto directory",
            },
            Case {
                name: "file_as_root",
                files: &[("file.proto", UNARY)],
                directories: &[],
                protos: &["file.proto"],
                imports: &[],
                error: "not a directory",
            },
            Case {
                name: "empty_directory",
                files: &[],
                directories: &["empty"],
                protos: &["empty"],
                imports: &[],
                error: "no .proto files",
            },
            Case {
                name: "missing_import_directory",
                files: &[("schema/service.proto", UNARY)],
                directories: &[],
                protos: &["schema"],
                imports: &["missing"],
                error: "import directory",
            },
            Case {
                name: "file_as_import_directory",
                files: &[("schema/service.proto", UNARY)],
                directories: &[],
                protos: &["schema"],
                imports: &["schema/service.proto"],
                error: "not a directory",
            },
            Case {
                name: "syntax_error",
                files: &[(
                    "schema/broken.proto",
                    "syntax = \"proto3\"; invalid schema;",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "broken.proto",
            },
            Case {
                name: "unresolved_import",
                files: &[(
                    "schema/broken.proto",
                    "syntax = \"proto3\"; import \"missing.proto\";",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "missing.proto",
            },
            Case {
                name: "ambiguous_entry_name",
                files: &[("one/service.proto", UNARY), ("two/service.proto", OTHER)],
                directories: &[],
                protos: &["two", "one"],
                imports: &[],
                error: "ambiguous proto import name `service.proto`",
            },
            Case {
                name: "ambiguous_import_name",
                files: &[
                    ("schema/app.proto", APP),
                    ("one/dependency.proto", DEPENDENCY),
                    ("two/dependency.proto", DEPENDENCY),
                ],
                directories: &[],
                protos: &["schema"],
                imports: &["one", "two"],
                error: "ambiguous proto import name `dependency.proto`",
            },
            Case {
                name: "client_streaming_application",
                files: &[(
                    "schema/service.proto",
                    "syntax = \"proto3\"; message Empty {} service Test { rpc Call(stream Empty) returns (Empty); }",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "streaming method /Test/Call",
            },
            Case {
                name: "server_streaming_application",
                files: &[(
                    "schema/service.proto",
                    "syntax = \"proto3\"; message Empty {} service Test { rpc Call(Empty) returns (stream Empty); }",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "streaming method /Test/Call",
            },
            Case {
                name: "bidirectional_streaming_application",
                files: &[(
                    "schema/service.proto",
                    "syntax = \"proto3\"; message Empty {} service Test { rpc Call(stream Empty) returns (stream Empty); }",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "streaming method /Test/Call",
            },
            Case {
                name: "reflection_v1_conflict",
                files: &[(
                    "schema/reflection.proto",
                    "syntax = \"proto3\"; package grpc.reflection.v1; service ServerReflection {}",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "reserved reflection service grpc.reflection.v1.ServerReflection",
            },
            Case {
                name: "reflection_v1alpha_conflict",
                files: &[(
                    "schema/reflection.proto",
                    "syntax = \"proto3\"; package grpc.reflection.v1alpha; service ServerReflection {}",
                )],
                directories: &[],
                protos: &["schema"],
                imports: &[],
                error: "reserved reflection service grpc.reflection.v1alpha.ServerReflection",
            },
            Case {
                name: "imported_reflection_service_conflict",
                files: &[
                    (
                        "schema/service.proto",
                        "syntax = \"proto3\"; import \"reflection.proto\"; service Test { rpc Call(grpc.reflection.v1.Empty) returns (grpc.reflection.v1.Empty); }",
                    ),
                    (
                        "include/reflection.proto",
                        "syntax = \"proto3\"; package grpc.reflection.v1; message Empty {} service ServerReflection {}",
                    ),
                ],
                directories: &[],
                protos: &["schema"],
                imports: &["include"],
                error: "reserved reflection service grpc.reflection.v1.ServerReflection",
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(dir.path(), case.files);
            for directory in case.directories {
                fs::create_dir_all(dir.path().join(directory)).unwrap();
            }
            let protos = case
                .protos
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let imports = case
                .imports
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let error = Registry::load(&protos, &imports).expect_err(case.name);
            assert!(
                format!("{error:#}").contains(case.error),
                "{}: {error:#}",
                case.name
            );
        }
    }

    #[test]
    fn rejects_unreadable_directories() {
        struct Case {
            name: &'static str,
            unreadable: &'static str,
        }
        let cases = [
            Case {
                name: "unreadable_proto_root",
                unreadable: "schema",
            },
            Case {
                name: "unreadable_nested_directory",
                unreadable: "schema/nested",
            },
            Case {
                name: "unreadable_import_root",
                unreadable: "include",
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(
                dir.path(),
                &[
                    ("schema/nested/app.proto", APP),
                    ("include/dependency.proto", DEPENDENCY),
                ],
            );
            let path = dir.path().join(case.unreadable);
            let permissions = fs::metadata(&path).unwrap().permissions();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
            let result =
                Registry::load(&[dir.path().join("schema")], &[dir.path().join("include")]);
            let denied = fs::read_dir(&path).is_err();
            fs::set_permissions(&path, permissions).unwrap();
            if denied {
                let error = result.expect_err(case.name);
                assert!(
                    format!("{error:#}").contains("reading proto directory"),
                    "{}: {error:#}",
                    case.name
                );
                assert!(
                    format!("{error:#}").contains(case.unreadable),
                    "{}: {error:#}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn preserves_custom_option_bytes() {
        struct Case {
            name: &'static str,
            marker: &'static str,
            option: &'static str,
        }
        let cases = [Case {
            name: "custom_method_option",
            marker: "rapira-custom-method-option",
            option: "rpc Call(Empty) returns (Empty) { option (audit.note) = \"rapira-custom-method-option\"; }",
        }];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            write_files(
                dir.path(),
                &[(
                    "include/options.proto",
                    r#"
                syntax = "proto2";
                package audit;
                import "google/protobuf/descriptor.proto";
                extend google.protobuf.MethodOptions { optional string note = 50001; }
            "#,
                )],
            );
            write_files(
                dir.path(),
                &[(
                    "schema/service.proto",
                    &format!(
                        r#"
                syntax = "proto3";
                import "options.proto";
                message Empty {{}}
                service Test {{ {} }}
            "#,
                        case.option
                    ),
                )],
            );
            let registry =
                Registry::load(&[dir.path().join("schema")], &[dir.path().join("include")])
                    .expect(case.name);
            let bytes = registry.encoded_descriptors();
            assert!(
                bytes
                    .windows(case.marker.len())
                    .any(|window| window == case.marker.as_bytes()),
                "{}",
                case.name
            );
            let decoded = FileDescriptorSet::decode(bytes).unwrap();
            assert!(
                decoded
                    .file
                    .iter()
                    .any(|file| file.name() == "google/protobuf/descriptor.proto"),
                "{}",
                case.name
            );
            assert!(
                !decoded
                    .encode_to_vec()
                    .windows(case.marker.len())
                    .any(|window| window == case.marker.as_bytes()),
                "{}",
                case.name
            );
        }
    }
}
