use anyhow::{Context, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct Schema {
    pub(crate) encoded_descriptors: Vec<u8>,
    pub(crate) entry_names: BTreeSet<String>,
}

pub(crate) fn compile(protos: &[PathBuf], import_paths: &[PathBuf]) -> anyhow::Result<Schema> {
    ensure!(
        !protos.is_empty(),
        "at least one proto directory is required"
    );
    let mut roots = normalize_roots(protos, "proto")?;
    let proto_root_count = roots.len();
    let import_roots = normalize_roots(import_paths, "import")?;
    for root in import_roots {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let mut sources = BTreeMap::<String, PathBuf>::new();
    let mut entry_files = BTreeSet::new();
    let mut entries = BTreeMap::new();
    for (index, root) in roots.iter().enumerate() {
        for path in discover(root)? {
            let name = path
                .strip_prefix(root)?
                .to_str()
                .with_context(|| format!("proto file name is not UTF-8: {}", path.display()))?;
            if let Some(other) = sources.get(name) {
                ensure!(
                    *other == path,
                    "ambiguous proto import name `{name}`: {} and {}",
                    other.display(),
                    path.display()
                );
            } else {
                sources.insert(name.to_owned(), path.clone());
            }
            if index < proto_root_count && entry_files.insert(path.clone()) {
                entries.insert(name.to_owned(), path);
            }
        }
        if index + 1 == proto_root_count {
            ensure!(
                !entries.is_empty(),
                "no .proto files found in proto directories"
            );
        }
    }

    let entry_names = entries.keys().cloned().collect();
    let mut compiler = protox::Compiler::new(roots)?;
    compiler.include_imports(true);
    for path in entries.into_values() {
        compiler
            .open_file(&path)
            .with_context(|| format!("parsing protobuf schema {}", path.display()))?;
    }
    // Preserve custom options in the encoded descriptors.
    // https://docs.rs/protox/0.9.1/protox/struct.Compiler.html#method.encode_file_descriptor_set
    let encoded_descriptors = compiler.encode_file_descriptor_set();
    Ok(Schema {
        encoded_descriptors,
        entry_names,
    })
}

fn normalize_roots(paths: &[PathBuf], kind: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for path in paths {
        let root = path
            .canonicalize()
            .with_context(|| format!("resolving {kind} directory {}", path.display()))?;
        ensure!(
            root.is_dir(),
            "{kind} path {} is not a directory",
            path.display()
        );
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok(roots)
}

fn discover(root: &Path) -> anyhow::Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();
    walk(root, &mut BTreeSet::new(), &mut files)?;
    Ok(files)
}

fn walk(
    directory: &Path,
    active: &mut BTreeSet<PathBuf>,
    files: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let canonical = directory
        .canonicalize()
        .with_context(|| format!("resolving proto directory {}", directory.display()))?;
    if !active.insert(canonical.clone()) {
        return Ok(());
    }
    let entries = fs::read_dir(directory)
        .with_context(|| format!("reading proto directory {}", directory.display()))?;
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("reading proto directory {}", directory.display()))?;
    paths.sort();
    for path in paths {
        let metadata = fs::metadata(&path)
            .with_context(|| format!("reading proto path {}", path.display()))?;
        if metadata.is_dir() {
            walk(&path, active, files)?;
        } else if metadata.is_file() && path.extension().is_some_and(|ext| ext == "proto") {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("resolving proto file {}", path.display()))?;
            ensure!(
                path == canonical,
                "symlink proto file alias {} -> {} is not supported; use the target file directly",
                path.display(),
                canonical.display()
            );
            files.insert(path);
        }
    }
    active.remove(&canonical);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn normalizes_roots_without_changing_namespace_order() {
        struct Case {
            name: &'static str,
            paths: &'static [&'static str],
            expected: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "nested_root_precedes_ancestor",
                paths: &["proto/vendor/googleapis", "proto"],
                expected: &["proto/vendor/googleapis", "proto"],
            },
            Case {
                name: "configuration_order_precedes_path_sort",
                paths: &["z_vendor", "a_vendor"],
                expected: &["z_vendor", "a_vendor"],
            },
            Case {
                name: "equivalent_paths_keep_first_position",
                paths: &["proto", "proto/./", "z_vendor", "proto"],
                expected: &["proto", "z_vendor"],
            },
            Case {
                name: "equivalent_symlink_root",
                paths: &["alias", "proto", "z_vendor"],
                expected: &["proto", "z_vendor"],
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("proto/vendor/googleapis")).unwrap();
            fs::create_dir_all(dir.path().join("a_vendor")).unwrap();
            fs::create_dir_all(dir.path().join("z_vendor")).unwrap();
            symlink(dir.path().join("proto"), dir.path().join("alias")).unwrap();
            let paths = case
                .paths
                .iter()
                .map(|path| dir.path().join(path))
                .collect::<Vec<_>>();
            let canonical = dir.path().canonicalize().unwrap();
            let expected = case
                .expected
                .iter()
                .map(|path| canonical.join(path))
                .collect::<Vec<_>>();
            assert_eq!(
                normalize_roots(&paths, "import").expect(case.name),
                expected,
                "{}",
                case.name
            );
        }
    }
}
