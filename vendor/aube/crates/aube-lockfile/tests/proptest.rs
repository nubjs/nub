use aube_lockfile::{
    DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph, LockfileKind, LockfileSettings,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
struct GraphShape {
    package_count: usize,
    edge_bits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedGraph {
    importers: BTreeMap<String, Vec<NormalizedDirectDep>>,
    packages: BTreeMap<String, NormalizedPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedDirectDep {
    name: String,
    dep_path: String,
    dep_type: DepType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedPackage {
    name: String,
    version: String,
    dependencies: BTreeMap<String, String>,
    optional_dependencies: BTreeMap<String, String>,
}

fn graph_shapes() -> impl Strategy<Value = GraphShape> {
    (1usize..=8).prop_flat_map(|package_count| {
        let edge_count = package_count * (package_count - 1) / 2;
        let edge_space = 1u32 << edge_count;
        (Just(package_count), 0..edge_space).prop_map(|(package_count, edge_bits)| GraphShape {
            package_count,
            edge_bits,
        })
    })
}

fn graph_from_shape(shape: GraphShape) -> (LockfileGraph, aube_manifest::PackageJson) {
    let mut manifest = aube_manifest::PackageJson {
        name: Some("roundtrip-root".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    let mut graph = LockfileGraph {
        settings: LockfileSettings::default(),
        ..Default::default()
    };

    for idx in 0..shape.package_count {
        let name = package_name(idx);
        let version = package_version(idx);
        let dep_path = dep_path(&name, &version);
        let dep_type = dep_type(idx);

        match dep_type {
            DepType::Production => {
                manifest.dependencies.insert(name.clone(), version.clone());
            }
            DepType::Dev => {
                manifest
                    .dev_dependencies
                    .insert(name.clone(), version.clone());
            }
            DepType::Optional => {
                manifest
                    .optional_dependencies
                    .insert(name.clone(), version.clone());
            }
        }

        graph
            .importers
            .entry(".".to_string())
            .or_default()
            .push(DirectDep {
                name: name.clone(),
                dep_path: dep_path.clone(),
                dep_type,
                specifier: Some(version.clone()),
            });

        let mut pkg = LockedPackage {
            name: name.clone(),
            version: version.clone(),
            integrity: Some(format!("sha512-{idx:02x}")),
            dep_path: dep_path.clone(),
            ..Default::default()
        };

        for prior_idx in 0..idx {
            let bit_idx = idx * (idx - 1) / 2 + prior_idx;
            if shape.edge_bits & (1 << bit_idx) == 0 {
                continue;
            }
            let child_name = package_name(prior_idx);
            let child_version = package_version(prior_idx);
            pkg.dependencies
                .insert(child_name.clone(), child_version.clone());
            pkg.declared_dependencies.insert(child_name, child_version);
        }

        graph.packages.insert(dep_path, pkg);
    }

    (graph, manifest)
}

fn package_name(idx: usize) -> String {
    format!("pkg-{idx}")
}

fn package_version(idx: usize) -> String {
    format!("1.0.{idx}")
}

fn dep_path(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

fn dep_type(idx: usize) -> DepType {
    match idx % 3 {
        1 => DepType::Dev,
        2 => DepType::Optional,
        _ => DepType::Production,
    }
}

fn normalize(graph: &LockfileGraph) -> NormalizedGraph {
    let importers = graph
        .importers
        .iter()
        .map(|(path, deps)| {
            let mut deps: Vec<_> = deps
                .iter()
                .map(|dep| NormalizedDirectDep {
                    name: dep.name.clone(),
                    dep_path: dep.dep_path.clone(),
                    dep_type: dep.dep_type,
                })
                .collect();
            deps.sort_by(|a, b| {
                (&a.name, &a.dep_path, dep_type_rank(a.dep_type)).cmp(&(
                    &b.name,
                    &b.dep_path,
                    dep_type_rank(b.dep_type),
                ))
            });
            (path.clone(), deps)
        })
        .collect();
    let packages = graph
        .packages
        .iter()
        .map(|(dep_path, pkg)| {
            (
                dep_path.clone(),
                NormalizedPackage {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    dependencies: normalize_dep_edges(&pkg.dependencies),
                    optional_dependencies: normalize_dep_edges(&pkg.optional_dependencies),
                },
            )
        })
        .collect();

    NormalizedGraph {
        importers,
        packages,
    }
}

fn normalize_dep_edges(deps: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    deps.iter()
        .map(|(name, value)| {
            let full_key = if value.starts_with(&format!("{name}@")) {
                value.clone()
            } else {
                dep_path(name, value)
            };
            (name.clone(), full_key)
        })
        .collect()
}

fn dep_type_rank(dep_type: DepType) -> u8 {
    match dep_type {
        DepType::Production => 0,
        DepType::Dev => 1,
        DepType::Optional => 2,
    }
}

fn roundtrip(
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    kind: LockfileKind,
) -> NormalizedGraph {
    let dir = tempfile::tempdir().expect("tempdir");
    aube_lockfile::write_lockfile_as(dir.path(), graph, manifest, kind).expect("write lockfile");
    let reparsed = aube_lockfile::parse_lockfile(dir.path(), manifest).expect("parse lockfile");
    normalize(&reparsed)
}

proptest! {
    #[test]
    fn npm_parser_classifies_declared_remote_tarballs(
        nested in any::<bool>(),
        scheme in prop_oneof![Just("http"), Just("https")],
        host in "[a-z][a-z0-9-]{0,10}",
        first_path in "[a-z][a-z0-9-]{0,10}",
        second_path in "[a-z][a-z0-9-]{0,10}",
        suffix in prop_oneof![Just(".tgz"), Just(".tar.gz?download=1"), Just("/download")],
        integrity_tail in "[A-Za-z0-9]{8,24}",
    ) {
        let url = format!(
            "{scheme}://{host}.example.test/{first_path}/{second_path}{suffix}"
        );
        let integrity = format!("sha512-{integrity_tail}");
        let remote_path = if nested {
            "node_modules/parent/node_modules/remote-pkg"
        } else {
            "node_modules/remote-pkg"
        };

        let mut packages = serde_json::Map::new();
        let root_dependencies = if nested {
            serde_json::json!({
                "parent": "1.0.0",
                "registry-pkg": "^2.0.0"
            })
        } else {
            serde_json::json!({
                "remote-pkg": url.clone(),
                "registry-pkg": "^2.0.0"
            })
        };
        packages.insert(
            String::new(),
            serde_json::json!({ "dependencies": root_dependencies }),
        );
        if nested {
            packages.insert(
                "node_modules/parent".to_string(),
                serde_json::json!({
                    "version": "1.0.0",
                    "dependencies": { "remote-pkg": url.clone() }
                }),
            );
        }
        packages.insert(
            remote_path.to_string(),
            serde_json::json!({
                "version": "3.0.0",
                "resolved": url.clone(),
                "integrity": integrity.clone()
            }),
        );
        packages.insert(
            "node_modules/registry-pkg".to_string(),
            serde_json::json!({
                "version": "2.1.0",
                "resolved": "https://registry.npmjs.org/registry-pkg/-/registry-pkg-2.1.0.tgz",
                "integrity": "sha512-registry"
            }),
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let lockfile = serde_json::json!({
            "lockfileVersion": 3,
            "packages": packages
        });
        std::fs::write(
            dir.path().join("package-lock.json"),
            serde_json::to_vec(&lockfile).expect("serialize lockfile"),
        )
        .expect("write lockfile");

        let graph = aube_lockfile::parse_lockfile(
            dir.path(),
            &aube_manifest::PackageJson::default(),
        )
        .expect("parse lockfile");
        let remote = graph
            .packages
            .values()
            .find(|pkg| pkg.name == "remote-pkg")
            .expect("remote package");
        let Some(LocalSource::RemoteTarball(source)) = &remote.local_source else {
            return Err(TestCaseError::fail(format!(
                "expected remote tarball source, got {:?}",
                remote.local_source
            )));
        };
        prop_assert_eq!(source.url.as_str(), url.as_str());
        prop_assert_eq!(source.integrity.as_str(), integrity.as_str());

        let registry = graph
            .packages
            .values()
            .find(|pkg| pkg.name == "registry-pkg")
            .expect("registry package");
        prop_assert!(registry.local_source.is_none());
    }

    #[test]
    fn pnpm_lockfile_roundtrips_generated_registry_graph(shape in graph_shapes()) {
        let (graph, manifest) = graph_from_shape(shape);
        prop_assert_eq!(roundtrip(&graph, &manifest, LockfileKind::Pnpm), normalize(&graph));
    }

    #[test]
    fn npm_lockfile_roundtrips_generated_registry_graph(shape in graph_shapes()) {
        let (graph, manifest) = graph_from_shape(shape);
        prop_assert_eq!(roundtrip(&graph, &manifest, LockfileKind::Npm), normalize(&graph));
    }

    #[test]
    fn bun_lockfile_roundtrips_generated_registry_graph(shape in graph_shapes()) {
        let (graph, manifest) = graph_from_shape(shape);
        prop_assert_eq!(roundtrip(&graph, &manifest, LockfileKind::Bun), normalize(&graph));
    }

    #[test]
    fn yarn_classic_lockfile_roundtrips_generated_registry_graph(shape in graph_shapes()) {
        let (graph, manifest) = graph_from_shape(shape);
        prop_assert_eq!(roundtrip(&graph, &manifest, LockfileKind::Yarn), normalize(&graph));
    }

    #[test]
    fn yarn_berry_lockfile_roundtrips_generated_registry_graph(shape in graph_shapes()) {
        let (graph, manifest) = graph_from_shape(shape);
        prop_assert_eq!(roundtrip(&graph, &manifest, LockfileKind::YarnBerry), normalize(&graph));
    }
}
