//! How many copies of a package an install produces when its optional peers
//! are provided on some paths and not others. pnpm's lockfile is the reference
//! for every expectation here.

use super::*;
use crate::tests::make_packument;
use aube_lockfile::{LockedPackage, LockfileGraph};
use aube_manifest::PackageJson;
use aube_registry::Packument;
use std::collections::{BTreeMap, HashMap};

/// A package with the given optional peers and dependencies.
fn pkg(
    name: &str,
    version: &str,
    optional_peers: &[(&str, &str)],
    deps: &[(&str, &str)],
) -> (String, Packument) {
    let mut packument = make_packument(name, &[version], version);
    let meta = packument.versions.get_mut(version).unwrap();
    for (peer, range) in optional_peers {
        meta.peer_dependencies
            .insert((*peer).to_string(), (*range).to_string());
        meta.peer_dependencies_meta.insert(
            (*peer).to_string(),
            aube_registry::PeerDepMeta { optional: true },
        );
    }
    for (dep, range) in deps {
        meta.dependencies
            .insert((*dep).to_string(), (*range).to_string());
    }
    (name.to_string(), packument)
}

fn manifest(deps: &[(&str, &str)]) -> PackageJson {
    let mut manifest = PackageJson::default();
    for (dep, range) in deps {
        manifest
            .dependencies
            .insert((*dep).to_string(), (*range).to_string());
    }
    manifest
}

fn resolver(packuments: impl IntoIterator<Item = (String, Packument)>) -> Resolver {
    let client = Arc::new(aube_registry::client::RegistryClient::new(
        "http://127.0.0.1:0",
    ));
    let mut resolver = Resolver::new(client);
    resolver.cache.extend(packuments);
    resolver
}

async fn members(resolver: &mut Resolver, members: &[(&str, &[(&str, &str)])]) -> LockfileGraph {
    let mut manifests = vec![(".".to_string(), PackageJson::default())];
    manifests.extend(
        members
            .iter()
            .map(|(path, deps)| ((*path).to_string(), manifest(deps))),
    );
    resolver
        .resolve_workspace(&manifests, None, &HashMap::new())
        .await
        .expect("resolve failed")
}

fn copies<'g>(graph: &'g LockfileGraph, name: &str) -> Vec<&'g str> {
    graph
        .packages
        .values()
        .filter(|pkg| pkg.name == name)
        .map(|pkg| pkg.dep_path.as_str())
        .collect()
}

fn direct<'g>(graph: &'g LockfileGraph, importer: &str, name: &str) -> Option<&'g str> {
    graph.importers[importer]
        .iter()
        .find(|dep| dep.name == name)
        .map(|dep| dep.dep_path.as_str())
}

#[tokio::test]
async fn a_member_without_the_optional_peer_shares_the_copy_of_a_member_with_it() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("vite", "*")], &[]),
        pkg("vite", "7.0.0", &[], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("plugin", "1.0.0")]),
            ("packages/b", &[("plugin", "1.0.0"), ("vite", "7.0.0")]),
        ],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0(vite@7.0.0)"]);
    assert_eq!(
        direct(&graph, "packages/a", "plugin"),
        Some("plugin@1.0.0(vite@7.0.0)")
    );
    assert_eq!(
        direct(&graph, "packages/a", "vite"),
        None,
        "a hoisted peer binds packages but is never the importer's own dependency"
    );
}

#[tokio::test]
async fn an_optional_peer_binds_a_version_only_a_cousin_brought_in() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("vite", "*")], &[]),
        pkg("runner", "1.0.0", &[], &[("vite", "7.0.0")]),
        pkg("vite", "7.0.0", &[], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[("packages/a", &[("plugin", "1.0.0"), ("runner", "1.0.0")])],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0(vite@7.0.0)"]);
    assert_eq!(
        graph.packages["plugin@1.0.0(vite@7.0.0)"].optional_dependencies["vite"],
        "7.0.0",
    );
}

#[tokio::test]
async fn an_optional_peer_stays_missing_when_no_version_in_the_graph_satisfies_it() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("vite", "^6")], &[]),
        pkg("runner", "1.0.0", &[], &[("vite", "7.0.0")]),
        pkg("vite", "7.0.0", &[], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[("packages/a", &[("plugin", "1.0.0"), ("runner", "1.0.0")])],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0"]);
}

#[tokio::test]
async fn a_hoist_is_passed_over_when_the_importer_rejects_the_providers_own_peer() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("vite", "*")], &[]),
        pkg("runner", "1.0.0", &[], &[("vite", "7.0.0")]),
        pkg("rollup", "3.0.0", &[], &[]),
        pkg("vite", "7.0.0", &[("rollup", "^4")], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[(
            "packages/a",
            &[
                ("plugin", "1.0.0"),
                ("rollup", "3.0.0"),
                ("runner", "1.0.0"),
            ],
        )],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0"]);
}

/// The importer holds `x@2`, so its own view of a hoisted provider binds that,
/// while every package that uses the provider binds `x@1`. Nothing reaches the
/// variant the importer visited.
#[tokio::test]
async fn a_hoisted_providers_unused_variant_leaves_the_graph() {
    let x = make_packument("x", &["1.0.0", "2.0.0"], "2.0.0");
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("provider", "*")], &[("x", "1.0.0")]),
        pkg(
            "runner",
            "1.0.0",
            &[],
            &[("provider", "1.0.0"), ("x", "1.0.0")],
        ),
        pkg("provider", "1.0.0", &[("x", "*")], &[]),
        ("x".to_string(), x),
    ]);
    let graph = members(
        &mut resolver,
        &[(
            "packages/a",
            &[("plugin", "1.0.0"), ("runner", "1.0.0"), ("x", "2.0.0")],
        )],
    )
    .await;

    assert_eq!(copies(&graph, "provider"), ["provider@1.0.0(x@1.0.0)"]);
}

/// `p` is outside `parent`'s range, so member `a` cannot hoist it and its copy of
/// `parent` has fewer dependencies than `b`'s. Their `child` binds different
/// versions of `x` though, so the copy is not a subset and each member keeps its own.
#[tokio::test]
async fn a_copy_whose_child_binds_another_peer_version_is_not_a_subset() {
    let mut resolver = resolver([
        pkg("parent", "1.0.0", &[("p", "^2")], &[("child", "1.0.0")]),
        ("child".to_string(), required_peer("child", "x", "*")),
        pkg("p", "1.0.0", &[], &[]),
        (
            "x".to_string(),
            make_packument("x", &["1.0.0", "2.0.0"], "2.0.0"),
        ),
    ]);
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("parent", "1.0.0"), ("x", "1.0.0")]),
            (
                "packages/b",
                &[("p", "1.0.0"), ("parent", "1.0.0"), ("x", "2.0.0")],
            ),
        ],
    )
    .await;

    let child = |importer| {
        let parent = direct(&graph, importer, "parent").expect("a direct dependency");
        graph.packages[parent].dependencies["child"].as_str()
    };
    assert_eq!(child("packages/a"), "1.0.0(x@1.0.0)");
    assert_eq!(child("packages/b"), "1.0.0(x@2.0.0)");
}

/// A peer's own peer keeps copies apart too. `@vitejs/plugin-basic-ssl` peers on
/// `vite`, and two members on different versions of `vite`'s `terser` need one copy
/// each, or one member's plugin imports the other member's `vite`.
#[tokio::test]
async fn members_on_different_versions_of_a_peers_peer_keep_two_copies() {
    let x = make_packument("x", &["1.0.0", "2.0.0"], "2.0.0");
    let mut resolver = resolver([
        ("plugin".to_string(), required_peer("plugin", "p", "*")),
        pkg("p", "1.0.0", &[("x", "*")], &[]),
        ("x".to_string(), x),
    ]);
    let graph = members(
        &mut resolver,
        &[
            (
                "packages/a",
                &[("plugin", "1.0.0"), ("p", "1.0.0"), ("x", "1.0.0")],
            ),
            (
                "packages/b",
                &[("plugin", "1.0.0"), ("p", "1.0.0"), ("x", "2.0.0")],
            ),
        ],
    )
    .await;

    let bound = |importer| {
        let plugin = direct(&graph, importer, "plugin").expect("a direct dependency");
        graph.packages[plugin].dependencies["p"].as_str()
    };
    assert_eq!(bound("packages/a"), "1.0.0(x@1.0.0)");
    assert_eq!(bound("packages/b"), "1.0.0(x@2.0.0)");
}

/// The subset can sit one level down: `a`'s `child` misses the peer `b`'s resolved,
/// so `child` collapses, and `parent`, which differs only by that child, with it.
#[tokio::test]
async fn a_copy_whose_child_is_a_subset_collapses_with_it() {
    let mut resolver = resolver([
        pkg("parent", "1.0.0", &[("p", "^2")], &[("child", "1.0.0")]),
        pkg("child", "1.0.0", &[("vite", ">=6")], &[]),
        pkg("p", "1.0.0", &[], &[]),
        pkg("vite", "5.0.0", &[], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("parent", "1.0.0")]),
            (
                "packages/b",
                &[("p", "1.0.0"), ("parent", "1.0.0"), ("vite", "5.0.0")],
            ),
        ],
    )
    .await;

    assert_eq!(copies(&graph, "child"), ["child@1.0.0(vite@5.0.0)"]);
    assert_eq!(copies(&graph, "parent").len(), 1);
}

/// Each side of a dependency cycle differs from its larger copy only by the other
/// side, so neither is identical to anything and only comparing them as a pair can
/// collapse them. A pair reached twice counts as fitting, as in pnpm.
#[tokio::test]
async fn copies_in_a_dependency_cycle_collapse_together() {
    let mut resolver = resolver([
        pkg("left", "1.0.0", &[("vite", ">=6")], &[("right", "1.0.0")]),
        pkg("right", "1.0.0", &[("vite", ">=6")], &[("left", "1.0.0")]),
        pkg("vite", "5.0.0", &[], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("left", "1.0.0")]),
            ("packages/b", &[("left", "1.0.0"), ("vite", "5.0.0")]),
        ],
    )
    .await;

    assert_eq!(
        copies(&graph, "left").len(),
        1,
        "{:?}",
        copies(&graph, "left")
    );
    assert_eq!(
        copies(&graph, "right").len(),
        1,
        "{:?}",
        copies(&graph, "right")
    );
}

/// `vite` is outside the range, so member `a` cannot hoist it, but the package
/// resolved it for member `b` anyway. `a`'s copy is then a subset of `b`'s and
/// collapses into it (`dedupePeerDependents`).
#[tokio::test]
async fn a_copy_missing_a_peer_collapses_into_the_copy_that_resolved_it() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("vite", ">=6")], &[]),
        pkg("vite", "5.0.0", &[], &[]),
    ]);
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("plugin", "1.0.0")]),
            ("packages/b", &[("plugin", "1.0.0"), ("vite", "5.0.0")]),
        ],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0(vite@5.0.0)"]);
    assert_eq!(
        direct(&graph, "packages/a", "plugin"),
        Some("plugin@1.0.0(vite@5.0.0)")
    );
}

/// Off, `dedupe-peer-dependents` keeps each importer's own resolution. The
/// hoist still runs: pnpm gates it on either of the two settings.
#[tokio::test]
async fn turning_off_dedupe_peer_dependents_keeps_the_subset_copy() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("vite", ">=6")], &[]),
        pkg("vite", "5.0.0", &[], &[]),
    ])
    .with_dedupe_peer_dependents(false);
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("plugin", "1.0.0")]),
            ("packages/b", &[("plugin", "1.0.0"), ("vite", "5.0.0")]),
        ],
    )
    .await;

    assert_eq!(
        copies(&graph, "plugin"),
        ["plugin@1.0.0", "plugin@1.0.0(vite@5.0.0)"]
    );
}

/// The other half of the contract: a peer the package declares itself splits it
/// across consumers on different versions, under pnpm and here, optional or not.
#[tokio::test]
async fn members_on_different_versions_of_a_declared_peer_keep_two_copies() {
    for optional in [true, false] {
        let mut plugin = pkg("plugin", "1.0.0", &[("vite", "*")], &[]);
        if !optional {
            plugin
                .1
                .versions
                .get_mut("1.0.0")
                .unwrap()
                .peer_dependencies_meta
                .clear();
        }
        let vite = make_packument("vite", &["6.0.0", "7.0.0"], "7.0.0");
        let mut resolver = resolver([plugin, ("vite".to_string(), vite)]);
        let graph = members(
            &mut resolver,
            &[
                ("packages/a", &[("plugin", "1.0.0"), ("vite", "6.0.0")]),
                ("packages/b", &[("plugin", "1.0.0"), ("vite", "7.0.0")]),
            ],
        )
        .await;

        assert_eq!(
            copies(&graph, "plugin"),
            ["plugin@1.0.0(vite@6.0.0)", "plugin@1.0.0(vite@7.0.0)"],
            "optional: {optional}"
        );
    }
}

fn database_entry(selector: &str, peer: &str, range: &str) -> DependencyPolicy {
    DependencyPolicy {
        package_extensions: vec![PackageExtension {
            selector: selector.to_string(),
            database_peers: [(peer.to_string(), range.to_string())].into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn typescript() -> (String, Packument) {
    let packument = make_packument("typescript", &["5.8.3", "5.9.3"], "5.9.3");
    ("typescript".to_string(), packument)
}

/// A database entry repairs an import pnpm does not know about, so it must not
/// produce a second copy pnpm would not install.
#[tokio::test]
async fn a_database_peer_does_not_split_members_on_different_versions() {
    let mut resolver = resolver([pkg("types", "1.0.0", &[], &[]), typescript()])
        .with_dependency_policy(database_entry("types@*", "typescript", "*"));
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("types", "1.0.0"), ("typescript", "5.8.3")]),
            ("packages/b", &[("types", "1.0.0"), ("typescript", "5.9.3")]),
        ],
    )
    .await;

    assert_eq!(copies(&graph, "types"), ["types@1.0.0"]);
    let types = &graph.packages["types@1.0.0"];
    assert_eq!(
        types.dependencies["typescript"], "5.9.3",
        "the sibling a store-resident copy resolves its import from"
    );
    assert!(types.optional_dependencies.contains_key("typescript"));
    assert!(
        types.peer_dependencies.is_empty(),
        "a repair is not a peer pnpm would write: {:?}",
        types.peer_dependencies
    );
}

/// `b` provides the target and `a` does not. They still share one copy, the way
/// a declared optional peer collapses, and `a`'s copy resolves the same target.
#[tokio::test]
async fn a_database_peer_does_not_split_a_member_without_the_target() {
    let mut resolver = resolver([pkg("types", "1.0.0", &[], &[]), typescript()])
        .with_dependency_policy(database_entry("types@*", "typescript", "*"));
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("types", "1.0.0")]),
            ("packages/b", &[("types", "1.0.0"), ("typescript", "5.9.3")]),
        ],
    )
    .await;

    assert_eq!(copies(&graph, "types"), ["types@1.0.0"]);
    assert_eq!(
        graph.packages["types@1.0.0"].dependencies["typescript"],
        "5.9.3"
    );
}

/// A target only a cousin brought in is not on the package's path, so the
/// package keeps resolving its import through its phantom eject. An edge here
/// would stop that eject, and take the target's `@types` package out of reach.
#[tokio::test]
async fn a_database_peer_does_not_bind_a_target_only_a_cousin_holds() {
    let mut resolver = resolver([
        pkg("types", "1.0.0", &[], &[]),
        pkg("runner", "1.0.0", &[], &[("typescript", "5.9.3")]),
        typescript(),
    ])
    .with_dependency_policy(database_entry("types@*", "typescript", "*"));
    let graph = members(
        &mut resolver,
        &[("packages/a", &[("runner", "1.0.0"), ("types", "1.0.0")])],
    )
    .await;

    assert!(graph.packages["types@1.0.0"].dependencies.is_empty());
}

#[tokio::test]
async fn a_database_peer_binds_nothing_when_the_graph_lacks_the_target() {
    let mut resolver = resolver([pkg("types", "1.0.0", &[], &[])])
        .with_dependency_policy(database_entry("types@*", "typescript", "*"));
    let graph = members(&mut resolver, &[("packages/a", &[("types", "1.0.0")])]).await;

    assert!(graph.packages["types@1.0.0"].dependencies.is_empty());
}

#[tokio::test]
async fn a_peer_the_package_declares_outranks_a_database_entry_for_it() {
    let mut resolver = resolver([
        pkg("types", "1.0.0", &[("typescript", "*")], &[]),
        typescript(),
    ])
    .with_dependency_policy(database_entry("types@*", "typescript", "*"));
    let graph = members(
        &mut resolver,
        &[
            ("packages/a", &[("types", "1.0.0"), ("typescript", "5.8.3")]),
            ("packages/b", &[("types", "1.0.0"), ("typescript", "5.9.3")]),
        ],
    )
    .await;

    assert_eq!(copies(&graph, "types").len(), 2);
}

/// A lockfile's edge is what the last resolve chose, not a pin: an upgraded
/// target must not stay installed just because the old edge named it.
#[tokio::test]
async fn a_lockfile_does_not_pin_a_database_peer() {
    let typescript_pkg = |version: &str| LockedPackage {
        name: "typescript".to_string(),
        version: version.to_string(),
        dep_path: format!("typescript@{version}"),
        ..Default::default()
    };
    let mut existing = LockfileGraph::default();
    existing.packages.insert(
        "types@1.0.0".to_string(),
        LockedPackage {
            name: "types".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "types@1.0.0".to_string(),
            dependencies: BTreeMap::from([("typescript".to_string(), "5.8.3".to_string())]),
            optional_dependencies: BTreeMap::from([(
                "typescript".to_string(),
                "5.8.3".to_string(),
            )]),
            ..Default::default()
        },
    );
    existing
        .packages
        .insert("typescript@5.8.3".to_string(), typescript_pkg("5.8.3"));

    let mut resolver = resolver([pkg("types", "1.0.0", &[], &[]), typescript()])
        .with_dependency_policy(database_entry("types@*", "typescript", "*"));
    let graph = resolver
        .resolve(
            &manifest(&[("types", "1.0.0"), ("typescript", "5.9.3")]),
            Some(&existing),
        )
        .await
        .expect("resolve failed");

    assert_eq!(
        graph.packages["types@1.0.0"].dependencies["typescript"],
        "5.9.3"
    );
    assert_eq!(
        copies(&graph, "typescript"),
        ["typescript@5.9.3"],
        "nothing else names the old target, so it must leave the graph"
    );
}

/// A lockfile holding `host` with a locked optional edge to `backend@1.0.0`.
fn locked_optional_edge() -> LockfileGraph {
    let mut existing = LockfileGraph::default();
    existing.packages.insert(
        "host@1.0.0".to_string(),
        LockedPackage {
            name: "host".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "host@1.0.0".to_string(),
            dependencies: BTreeMap::from([("backend".to_string(), "1.0.0".to_string())]),
            optional_dependencies: BTreeMap::from([("backend".to_string(), "1.0.0".to_string())]),
            ..Default::default()
        },
    );
    existing.packages.insert(
        "backend@1.0.0".to_string(),
        LockedPackage {
            name: "backend".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "backend@1.0.0".to_string(),
            ..Default::default()
        },
    );
    existing
}

/// A resolver where a database entry names `backend` for `host`, whose manifest
/// declares it as an optional dependency or does not.
fn resolver_for_locked_edge(declared: bool) -> Resolver {
    let mut host = make_packument("host", &["1.0.0"], "1.0.0");
    if declared {
        host.versions
            .get_mut("1.0.0")
            .unwrap()
            .optional_dependencies
            .insert("backend".to_string(), "^1.0.0".to_string());
    }
    resolver([
        ("host".to_string(), host),
        (
            "backend".to_string(),
            make_packument("backend", &["1.0.0", "2.0.0"], "2.0.0"),
        ),
    ])
    .with_dependency_policy(database_entry("host@*", "backend", "*"))
}

/// `sharp` has a database entry naming `@img/sharp-wasm32`, which it also declares
/// as optional, so the edge is not the repair even with nothing else holding the name.
#[tokio::test]
async fn a_lockfile_keeps_a_declared_optional_dependency_a_database_entry_also_names() {
    let graph = resolver_for_locked_edge(true)
        .resolve(
            &manifest(&[("host", "1.0.0")]),
            Some(&locked_optional_edge()),
        )
        .await
        .expect("resolve failed");

    assert_eq!(
        graph.packages["host@1.0.0"].dependencies["backend"],
        "1.0.0"
    );
    assert_eq!(copies(&graph, "backend"), ["backend@1.0.0"]);
}

/// Another version of the name in the graph does not make it a repair either: `host`
/// declares `backend` 1, which must not follow the root to 2.
#[tokio::test]
async fn a_lockfile_keeps_a_declared_optional_dependency_when_another_version_is_in_the_graph() {
    let graph = resolver_for_locked_edge(true)
        .resolve(
            &manifest(&[("backend", "2.0.0"), ("host", "1.0.0")]),
            Some(&locked_optional_edge()),
        )
        .await
        .expect("resolve failed");

    assert_eq!(
        graph.packages["host@1.0.0"].dependencies["backend"],
        "1.0.0"
    );
}

struct AddsBackendToHost;

impl ReadPackageHook for AddsBackendToHost {
    fn read_package<'a>(
        &'a mut self,
        mut pkg: aube_registry::VersionMetadata,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<aube_registry::VersionMetadata, String>>
                + Send
                + 'a,
        >,
    > {
        if pkg.name == "host" {
            pkg.optional_dependencies
                .insert("backend".to_string(), "^1.0.0".to_string());
        }
        Box::pin(async move { Ok(pkg) })
    }
}

/// A dependency `readPackage` adds counts as declared, as it does on a fresh resolve.
#[tokio::test]
async fn a_lockfile_keeps_an_optional_dependency_a_read_package_hook_adds() {
    let graph = resolver_for_locked_edge(false)
        .with_read_package_hook(Box::new(AddsBackendToHost))
        .resolve(
            &manifest(&[("backend", "2.0.0"), ("host", "1.0.0")]),
            Some(&locked_optional_edge()),
        )
        .await
        .expect("resolve failed");

    assert_eq!(
        graph.packages["host@1.0.0"].dependencies["backend"],
        "1.0.0"
    );
}

/// With its last provider gone, a repair must not keep the target installed: the same
/// manifest resolved from nothing has neither.
#[tokio::test]
async fn a_lockfile_drops_a_database_peer_when_its_last_provider_leaves() {
    let graph = resolver_for_locked_edge(false)
        .resolve(
            &manifest(&[("host", "1.0.0")]),
            Some(&locked_optional_edge()),
        )
        .await
        .expect("resolve failed");

    assert!(graph.packages["host@1.0.0"].dependencies.is_empty());
    assert!(copies(&graph, "backend").is_empty());
}

/// A repair stays on its locked target while that target is in the graph, even with
/// only another member holding it, so an unrelated change does not move it.
#[tokio::test]
async fn a_lockfile_keeps_a_database_peer_while_its_target_stays() {
    let graph = resolver_for_locked_edge(false)
        .resolve_workspace(
            &[
                (".".to_string(), PackageJson::default()),
                ("packages/a".to_string(), manifest(&[("host", "1.0.0")])),
                ("packages/b".to_string(), manifest(&[("backend", "1.0.0")])),
            ],
            Some(&locked_optional_edge()),
            &HashMap::new(),
        )
        .await
        .expect("resolve failed");

    assert_eq!(
        graph.packages["host@1.0.0"].dependencies["backend"],
        "1.0.0"
    );
}

/// `nx` holds `supports-color` 7 and has a database entry for `@nx/js`, whose
/// subtree reaches `debug`. pnpm has no such edge, so that `debug` takes the 8 the
/// hoist put on its path, and neither `nx`'s dependencies nor `@nx/js`'s peers
/// change anything's identity. `atool` sorts first because the pass visits a
/// package without peers of its own once, from wherever it reaches it first.
#[tokio::test]
async fn a_database_edge_changes_no_peer_binding_in_its_targets_subtree() {
    let mut resolver = resolver([
        pkg("atool", "1.0.0", &[], &[("color", "7.0.0")]),
        pkg("js", "1.0.0", &[], &[("dbg", "1.0.0")]),
        pkg("dbg", "1.0.0", &[("color", "*")], &[]),
        pkg("worker", "1.0.0", &[], &[("color", "8.0.0")]),
        (
            "color".to_string(),
            make_packument("color", &["7.0.0", "8.0.0"], "8.0.0"),
        ),
    ])
    .with_dependency_policy(database_entry("atool@*", "js", "*"));
    let graph = members(
        &mut resolver,
        &[(
            "packages/a",
            &[("atool", "1.0.0"), ("js", "1.0.0"), ("worker", "1.0.0")],
        )],
    )
    .await;

    assert_eq!(copies(&graph, "dbg"), ["dbg@1.0.0(color@8.0.0)"]);
    assert_eq!(copies(&graph, "atool"), ["atool@1.0.0"]);
    assert!(
        graph.packages["atool@1.0.0"].dependencies["js"].starts_with("1.0.0"),
        "the import stays repaired"
    );
}

/// A required peer nothing on the path provides is hoisted first, so an optional
/// peer of the same name binds that version rather than the highest in the graph.
/// `ts-jest` beside a package needing `@babel/core@^7` and another bringing 8.
#[tokio::test]
async fn a_missing_required_peer_decides_the_version_an_optional_one_binds() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("core", ">=7 <9")], &[]),
        pkg("runner", "1.0.0", &[], &[("needs-core", "1.0.0")]),
        (
            "needs-core".to_string(),
            required_peer("needs-core", "core", "^7.0.0"),
        ),
        pkg("other", "1.0.0", &[], &[("core", "8.0.0")]),
        (
            "core".to_string(),
            make_packument("core", &["7.0.0", "8.0.0"], "8.0.0"),
        ),
    ]);
    let graph = members(
        &mut resolver,
        &[(
            "packages/a",
            &[("other", "1.0.0"), ("plugin", "1.0.0"), ("runner", "1.0.0")],
        )],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0(core@7.0.0)"]);
    assert_eq!(
        copies(&graph, "needs-core"),
        ["needs-core@1.0.0(core@7.0.0)"]
    );
}

/// A package binding a peer from its parent decides the version for the rest of
/// the importer, ahead of a higher one in the graph. `styled-jsx`'s optional
/// `@babel/core` takes 7 beside `@vitejs/plugin-react`, which holds 7 and has a
/// plugin peering on it, while `@angular/compiler-cli` brings 8.
#[tokio::test]
async fn a_peer_bound_from_a_parent_decides_the_version_an_optional_one_binds() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("core", "*")], &[]),
        pkg(
            "holder",
            "1.0.0",
            &[],
            &[("core", "7.0.0"), ("needs-core", "1.0.0")],
        ),
        (
            "needs-core".to_string(),
            required_peer("needs-core", "core", ">=7 <9"),
        ),
        pkg("other", "1.0.0", &[], &[("core", "8.0.0")]),
        (
            "core".to_string(),
            make_packument("core", &["7.0.0", "8.0.0"], "8.0.0"),
        ),
    ]);
    let graph = members(
        &mut resolver,
        &[(
            "packages/a",
            &[("holder", "1.0.0"), ("other", "1.0.0"), ("plugin", "1.0.0")],
        )],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0(core@7.0.0)"]);
    assert_eq!(
        copies(&graph, "needs-core"),
        ["needs-core@1.0.0(core@7.0.0)"]
    );
}

/// Nothing binds the peer from a parent here, so the optional peer takes the
/// highest version, as `styled-jsx` beside `@angular/compiler-cli` alone does.
#[tokio::test]
async fn an_optional_peer_nobody_bound_takes_the_highest_version() {
    let mut resolver = resolver([
        pkg("plugin", "1.0.0", &[("core", "*")], &[]),
        pkg("holder", "1.0.0", &[], &[("core", "7.0.0")]),
        pkg("other", "1.0.0", &[], &[("core", "8.0.0")]),
        (
            "core".to_string(),
            make_packument("core", &["7.0.0", "8.0.0"], "8.0.0"),
        ),
    ]);
    let graph = members(
        &mut resolver,
        &[(
            "packages/a",
            &[("holder", "1.0.0"), ("other", "1.0.0"), ("plugin", "1.0.0")],
        )],
    )
    .await;

    assert_eq!(copies(&graph, "plugin"), ["plugin@1.0.0(core@8.0.0)"]);
}

fn required_peer(name: &str, peer: &str, range: &str) -> Packument {
    let mut packument = make_packument(name, &["1.0.0"], "1.0.0");
    packument
        .versions
        .get_mut("1.0.0")
        .unwrap()
        .peer_dependencies
        .insert(peer.to_string(), range.to_string());
    packument
}
