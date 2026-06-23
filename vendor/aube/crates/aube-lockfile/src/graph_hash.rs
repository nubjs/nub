//! Content-addressed virtual store path computation.
//!
//! Ports pnpm's `calcGraphNodeHash` from `/tmp/pnpm/deps/graph-hasher/` —
//! the mechanism that lets pnpm's global virtual store safely share
//! built packages across projects. The core idea:
//!
//! 1. Each lockfile node gets a **dep-graph hash** derived from its own
//!    identity (the integrity hash / fullPkgId) plus the recursively
//!    hashed dep-graph subtree. Two projects whose resolution produces
//!    the same `(foo, [same children, same versions, same identities])`
//!    end up with the same hash, so they share a virtual-store entry.
//! 2. For packages that **transitively depend on anything allowed to
//!    run build scripts**, the hash also folds in an engine string
//!    (os/arch/node-version). Building a native module against node 20
//!    produces a different hash than building it against node 22, so
//!    the two artifacts live at different paths and never collide.
//! 3. Everything else (pure-JS packages whose subtree contains nothing
//!    that builds) has a hash of `engine=null` — stable across
//!    architectures, so pure-JS trees are still shared globally.
//!
//! Unlike pnpm, we use BLAKE3 over a canonical JSON serialization —
//! aube's virtual store is internal to aube (the CAS under
//! `$XDG_DATA_HOME/aube/store/v1/files` is ours alone), so we don't
//! need bit-for-bit compatibility with pnpm's `object-hash`.
//! Determinism is all that matters, and `serde_json` plus `BTreeMap`
//! gives us alphabetized keys for free. BLAKE3 is the project default
//! for non-crypto-verifying hashes (3-5x faster than SHA-256).

use crate::{LockedPackage, LockfileGraph, dep_type_label, shared_local_dep_path};
use serde::Serialize;
use std::collections::BTreeMap;

/// Resolve a child dependency's recorded `(alias, tail)` to the graph
/// key the target package is stored under.
///
/// Registry deps record their version verbatim, so `alias@tail` is the
/// key. Git / remote-tarball deps record their *resolved URL* as the
/// tail while the package is keyed under the hashed
/// `alias@git+<hash>` / `alias@url+<hash>` form; [`shared_local_dep_path`]
/// performs that translation. Falling back to the raw `alias@tail`
/// keeps the common case allocation-light and behaves identically to
/// the pre-canonicalization lookup for everything that isn't a
/// content-pinned source.
///
/// Keeping this in lockstep with the linker's sibling-symlink keying
/// (which calls the same helper) is load-bearing: if the hasher skipped
/// a URL-shaped git child, the parent's GVS hash would omit that child's
/// content fingerprint and build/engine taint, and two materially
/// different trees would collide on one virtual-store path.
fn child_dep_path(alias: &str, tail: &str) -> String {
    shared_local_dep_path(alias, tail).unwrap_or_else(|| format!("{alias}@{tail}"))
}

use aube_util::collections::FxMap as FxHashMap;
use aube_util::collections::FxSet as FxHashSet;

/// A callback the caller provides to tell the hasher which
/// `(name, version)` combinations are allowed to run lifecycle
/// scripts. Implemented by `aube-scripts::BuildPolicy` in practice,
/// but the hasher stays oblivious to the policy crate so the lockfile
/// crate doesn't depend on it.
pub type AllowBuildFn<'a> = &'a dyn Fn(&LockedPackage) -> bool;

/// Engine fingerprint folded into a node's hash when any of its
/// transitive deps are allowed to build. Callers compute this once
/// per install; see [`engine_name_default`] for the standard format.
#[derive(Debug, Clone)]
pub struct EngineName(pub String);

/// `<os>-<arch>-node<major>` — e.g. `linux-x64-node20`. Enough to
/// distinguish builds across the axes that actually break native
/// modules. The arch string is translated from Rust's naming
/// (`x86_64`, `aarch64`) to Node's (`x64`, `arm64`) so the virtual
/// store directories look familiar next to `process.arch` output.
/// Libc detection is a known gap (TODO: musl vs glibc).
pub fn engine_name_default(node_version: &str) -> EngineName {
    let os = std::env::consts::OS;
    let arch = node_arch(std::env::consts::ARCH);
    let major = node_version
        .trim_start_matches('v')
        .split('.')
        .next()
        .unwrap_or("");
    EngineName(format!("{os}-{arch}-node{major}"))
}

/// Map Rust `std::env::consts::ARCH` values to Node's `process.arch`
/// convention. Unknown inputs pass through unchanged — better to leak
/// a Rust-flavored name into a debug path than to silently collapse
/// two distinct architectures onto the same bucket.
fn node_arch(rust_arch: &str) -> &str {
    match rust_arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "powerpc" => "ppc",
        other => other,
    }
}

/// Result of a full hashing pass over a `LockfileGraph`.
#[derive(Debug, Default, Clone)]
pub struct GraphHashes {
    /// Per-dep_path final hash used as the virtual-store subdir suffix.
    pub node_hash: BTreeMap<String, String>,
}

impl GraphHashes {
    /// Look up a hashed subdir name for `dep_path`, falling back to the
    /// raw dep_path when the hash is unknown. Callers threading this
    /// through the linker can use it as a drop-in for the bare
    /// dep_path when constructing virtual-store paths.
    pub fn hashed_dep_path(&self, dep_path: &str) -> String {
        match self.node_hash.get(dep_path) {
            Some(hex) => append_hex_to_leaf(dep_path, hex),
            None => dep_path.to_string(),
        }
    }
}

/// Append `-<hex>` to the final slash-separated component of `dep_path`.
/// For scoped packages like `@scope/name@ver` this preserves the scope
/// prefix and only decorates the leaf, so the existing 2-component
/// directory layout carries through unchanged except for a longer leaf
/// name.
fn append_hex_to_leaf(dep_path: &str, hex: &str) -> String {
    // 16 chars of sha256 hex = 64 bits, more than enough to avoid
    // collisions inside one project's lockfile (which typically has a
    // few thousand nodes at most). Using the full 64 would just make
    // paths awkward to stare at in `ls`.
    let short = &hex[..hex.len().min(16)];
    match dep_path.rfind('/') {
        Some(i) => format!("{}/{}-{}", &dep_path[..i], &dep_path[i + 1..], short),
        None => format!("{dep_path}-{short}"),
    }
}

/// Per-`(name, version)` patch fingerprint. Folded into `full_pkg_id`
/// so a patched node hashes differently from the unpatched one — and
/// because the recursive `calc_deps_hash` mixes child hashes into
/// every ancestor, every dep that transitively pulls in the patched
/// package also lands at a fresh virtual-store path.
pub type PatchHashFn<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

/// Per-`dep_path` materialized-content fingerprint. Folded into
/// `full_pkg_id` so a source-backed dependency (git / remote tarball)
/// whose lockfile coordinate is identical to another's but whose
/// on-disk bytes differ hashes to a distinct value.
///
/// The motivating case is a git dep installed once normally (its
/// `prepare` built `dist/`) and once under `--ignore-scripts` (raw
/// checkout): same `<url>#<commit>` coordinate, no integrity in the
/// lockfile, but different trees. Keying the global virtual store by
/// coordinate alone would let the first project's built tree leak into
/// the second's scripts-free install; folding the content fingerprint
/// in keeps them at separate paths. Returns `None` for packages whose
/// content the caller doesn't fingerprint (registry packages already
/// carry an integrity, so they need no extra disambiguation).
pub type ContentHashFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Compute final hashes for every package in `graph`. When
/// `engine` is `Some`, packages whose transitive subtree contains a
/// build-allowed package fold the engine name into their hash; when
/// `None` or when no package in the subtree is allowed to build, the
/// hash is engine-agnostic.
pub fn compute_graph_hashes(
    graph: &LockfileGraph,
    allow_build: AllowBuildFn<'_>,
    engine: Option<&EngineName>,
) -> GraphHashes {
    compute_graph_hashes_with_patches(graph, allow_build, engine, &|_, _| None)
}

/// Variant of [`compute_graph_hashes`] that also folds per-package
/// patch fingerprints into the hash, so patched packages live at
/// distinct virtual-store paths.
pub fn compute_graph_hashes_with_patches(
    graph: &LockfileGraph,
    allow_build: AllowBuildFn<'_>,
    engine: Option<&EngineName>,
    patch_hash: PatchHashFn<'_>,
) -> GraphHashes {
    compute_graph_hashes_full(graph, allow_build, engine, patch_hash, &|_| None)
}

/// Variant of [`compute_graph_hashes_with_patches`] that additionally
/// folds a per-`dep_path` materialized-content fingerprint into each
/// node's identity. See [`ContentHashFn`] for why this is needed for
/// source-backed (git / remote-tarball) dependencies under the global
/// virtual store.
pub fn compute_graph_hashes_full(
    graph: &LockfileGraph,
    allow_build: AllowBuildFn<'_>,
    engine: Option<&EngineName>,
    patch_hash: PatchHashFn<'_>,
    content_hash: ContentHashFn<'_>,
) -> GraphHashes {
    // Pass 1: identify every dep_path whose `(name, version)` is
    // allowed to run its scripts. This is the "builds" set.
    let mut builds: FxHashSet<String> = FxHashSet::default();
    for (dep_path, pkg) in &graph.packages {
        if allow_build(pkg) {
            builds.insert(dep_path.clone());
        }
    }

    // Pass 2: per-package dep-graph hash (recursive, memoized).
    let mut deps_hash_cache: FxHashMap<String, String> = FxHashMap::default();
    for dep_path in graph.packages.keys() {
        let _ = calc_deps_hash(
            graph,
            dep_path,
            &mut deps_hash_cache,
            &mut FxHashSet::default(),
            patch_hash,
            content_hash,
        );
    }

    // Pass 3: per-package "does the subtree transitively need engine
    // tainting?" cache.
    let mut requires_build_cache: FxHashMap<String, bool> = FxHashMap::default();
    for dep_path in graph.packages.keys() {
        transitively_requires_build(
            graph,
            &builds,
            dep_path,
            &mut requires_build_cache,
            &mut FxHashSet::default(),
        );
    }

    // Pass 4: final `node_hash(engine?, deps)` per package.
    let mut node_hash: BTreeMap<String, String> = BTreeMap::new();
    for dep_path in graph.packages.keys() {
        let include_engine =
            engine.is_some() && *requires_build_cache.get(dep_path).unwrap_or(&false);
        let engine_str = if include_engine {
            Some(engine.unwrap().0.as_str())
        } else {
            None
        };
        let deps_hash = deps_hash_cache.get(dep_path).cloned().unwrap_or_default();
        let hex = hash_canonical(&NodeHashInput {
            engine: engine_str,
            deps: &deps_hash,
        });
        node_hash.insert(dep_path.clone(), hex);
    }

    GraphHashes { node_hash }
}

/// A single 32-byte digest identifying the WHOLE resolved graph —
/// every package's recursive dep-graph hash plus every importer's
/// direct-dependency edges. Two graphs that resolve to the same set of
/// `(dep_path → identity)` package nodes AND the same importer edges
/// produce the same digest; any change to a package's identity, its
/// dependency wiring, the package set, or an importer's direct deps
/// flips it.
///
/// This is the equality primitive a no-churn write guard compares:
/// hash the freshly-resolved graph, hash the graph the on-disk lockfile
/// parses to, and skip the write when the two digests match. It is
/// deliberately engine-AGNOSTIC (`engine: None`) — the virtual-store
/// engine taint is a per-host materialization concern, not part of the
/// lockfile's recorded identity, so two hosts on different Node majors
/// must still see an unchanged lockfile as unchanged.
///
/// Order-independent by construction: package hashes are folded through
/// the sorted `node_hash` `BTreeMap`, and importer edges are serialized
/// from `BTreeMap`/sorted `Vec`s, so re-parsing a lockfile whose entries
/// landed in a different on-disk order yields the same digest.
pub fn graph_identity_hash(graph: &LockfileGraph, allow_build: AllowBuildFn<'_>) -> [u8; 32] {
    graph_identity_hash_with_patches(graph, allow_build, &|_, _| None)
}

/// [`graph_identity_hash`] variant that folds per-package patch
/// fingerprints into each node's identity, so a re-patched package
/// counts as a graph change (matching the delta path's treatment).
pub fn graph_identity_hash_with_patches(
    graph: &LockfileGraph,
    allow_build: AllowBuildFn<'_>,
    patch_hash: PatchHashFn<'_>,
) -> [u8; 32] {
    // Engine-agnostic: the recorded lockfile identity must not depend
    // on the host's os/arch/node-major.
    let hashes = compute_graph_hashes_with_patches(graph, allow_build, None, patch_hash);

    // Canonical, order-independent serialization of the parts that
    // define the graph: every package's identity hash, plus every
    // importer's sorted direct-dependency edges.
    #[derive(Serialize)]
    struct ImporterEdge<'a> {
        name: &'a str,
        dep_path: &'a str,
        dep_type: &'a str,
        specifier: Option<&'a str>,
    }
    #[derive(Serialize)]
    struct GraphIdentityInput<'a> {
        nodes: &'a BTreeMap<String, String>,
        importers: BTreeMap<&'a str, Vec<ImporterEdge<'a>>>,
    }

    let importers: BTreeMap<&str, Vec<ImporterEdge<'_>>> = graph
        .importers
        .iter()
        .map(|(path, deps)| {
            let mut edges: Vec<ImporterEdge<'_>> = deps
                .iter()
                .map(|d| ImporterEdge {
                    name: &d.name,
                    dep_path: &d.dep_path,
                    dep_type: dep_type_label(d.dep_type),
                    specifier: d.specifier.as_deref(),
                })
                .collect();
            // Direct deps are written in a stable order, but re-parsing
            // a foreign lockfile (e.g. pnpm's) could yield a different
            // edge order; sort so the digest is order-independent.
            edges.sort_by(|a, b| {
                (a.name, a.dep_path, a.dep_type, a.specifier).cmp(&(
                    b.name,
                    b.dep_path,
                    b.dep_type,
                    b.specifier,
                ))
            });
            (path.as_str(), edges)
        })
        .collect();

    let input = GraphIdentityInput {
        nodes: &hashes.node_hash,
        importers,
    };
    let json = serde_json::to_vec(&input).expect("graph identity input must serialize");
    *blake3::hash(&json).as_bytes()
}

/// Compute the recursive dep-graph hash for one package. Uses the
/// node's `full_pkg_id` (its integrity when present, else a stringified
/// fallback) plus a sorted map of `child_alias -> child_deps_hash`.
///
/// Cycle-safe: packages already on the current DFS stack return an
/// empty string, matching pnpm's behavior (the hash loses a small bit
/// of information for cyclic peer-dep contexts, but it stays stable
/// and deterministic).
fn calc_deps_hash(
    graph: &LockfileGraph,
    dep_path: &str,
    cache: &mut FxHashMap<String, String>,
    parents: &mut FxHashSet<String>,
    patch_hash: PatchHashFn<'_>,
    content_hash: ContentHashFn<'_>,
) -> String {
    if let Some(cached) = cache.get(dep_path) {
        return cached.clone();
    }
    if !parents.insert(dep_path.to_string()) {
        // Cycle: contribute an empty hash to break the recursion.
        // (Pnpm's version of this fans out from `fullPkgId` → `deps:{}`
        // when a node is already a parent; empty string here does the
        // same job via the canonical serializer.)
        return String::new();
    }

    let hash = match graph.packages.get(dep_path) {
        Some(pkg) => {
            let id = full_pkg_id(pkg, patch_hash, content_hash(dep_path).as_deref());
            let mut deps: BTreeMap<String, String> = BTreeMap::new();
            for (alias, child_tail) in &pkg.dependencies {
                let child_dep_path = child_dep_path(alias, child_tail);
                // The child might not be in the graph if the lockfile
                // has a dangling reference (e.g. after manual edits);
                // skip rather than panic.
                if !graph.packages.contains_key(&child_dep_path) {
                    continue;
                }
                let child_hash = calc_deps_hash(
                    graph,
                    &child_dep_path,
                    cache,
                    parents,
                    patch_hash,
                    content_hash,
                );
                deps.insert(alias.clone(), child_hash);
            }
            hash_canonical(&DepsHashInput {
                id: &id,
                deps: &deps,
            })
        }
        None => String::new(),
    };

    parents.remove(dep_path);
    cache.insert(dep_path.to_string(), hash.clone());
    hash
}

/// Returns `true` if `dep_path` is allowed to build, or if any of its
/// transitive children are. Mirrors pnpm's `transitivelyRequiresBuild`.
fn transitively_requires_build(
    graph: &LockfileGraph,
    builds: &FxHashSet<String>,
    dep_path: &str,
    cache: &mut FxHashMap<String, bool>,
    parents: &mut FxHashSet<String>,
) -> bool {
    if let Some(&cached) = cache.get(dep_path) {
        return cached;
    }
    if builds.contains(dep_path) {
        cache.insert(dep_path.to_string(), true);
        return true;
    }
    if !parents.insert(dep_path.to_string()) {
        return false;
    }
    let result = match graph.packages.get(dep_path) {
        Some(pkg) => pkg.dependencies.iter().any(|(alias, tail)| {
            let child_dep_path = child_dep_path(alias, tail);
            transitively_requires_build(graph, builds, &child_dep_path, cache, parents)
        }),
        None => false,
    };
    parents.remove(dep_path);
    cache.insert(dep_path.to_string(), result);
    result
}

/// The set of dep_paths whose final graph hash folds in a content
/// fingerprint: every globally-shareable source dependency (git /
/// remote tarball — see [`LocalSource::is_globally_shareable`]) plus
/// every package that transitively depends on one.
///
/// This exists to keep the GVS-prewarm materializer honest. Prewarm
/// runs *concurrently with fetch*, so it can't fingerprint source trees
/// that haven't been imported yet — it hashes with [`ContentHashFn`]
/// returning `None` for everything. The link phase runs after fetch and
/// folds the real fingerprints in via [`compute_graph_hashes_full`]. For
/// any package in this set the two passes compute *different* hashes, so
/// a node the prewarm materializes lands at a content-less path the link
/// phase never references — stranding a duplicate cohort in the global
/// store. Worse, prewarm skips the shareable-source *leaves* themselves
/// (it can't materialize an un-fetched tree), so that stranded cohort's
/// sibling symlinks dangle and Node's module walk silently resolves a
/// second copy of the package higher up the tree (a duplicate-singleton
/// "Cannot find module" class of bug). Prewarm must skip exactly this set
/// and defer it to the link phase, which materializes every node at its
/// final content-ful path.
///
/// Computed by reverse reachability from the shareable-source seeds so
/// it's order- and cycle-independent: a source-backed subtree can sit
/// inside a self-referential peer-dependency cycle, which a forward DFS
/// memo would mis-handle depending on traversal entry point.
///
/// [`LocalSource::is_globally_shareable`]: crate::LocalSource::is_globally_shareable
pub fn content_affected_dep_paths(graph: &LockfileGraph) -> FxHashSet<String> {
    let mut parents_of: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut stack: Vec<String> = Vec::new();
    for (dep_path, pkg) in &graph.packages {
        if pkg
            .local_source
            .as_ref()
            .is_some_and(|source| source.is_globally_shareable())
        {
            stack.push(dep_path.clone());
        }
        for (alias, child_tail) in &pkg.dependencies {
            let child = child_dep_path(alias, child_tail);
            if graph.packages.contains_key(&child) {
                parents_of.entry(child).or_default().push(dep_path.clone());
            }
        }
    }
    let mut affected: FxHashSet<String> = FxHashSet::default();
    while let Some(dep_path) = stack.pop() {
        if !affected.insert(dep_path.clone()) {
            continue;
        }
        if let Some(parents) = parents_of.get(&dep_path) {
            stack.extend(parents.iter().cloned());
        }
    }
    affected
}

/// `full_pkg_id` — pnpm uses `${pkgIdWithPatchHash}:${resolution}`; we
/// use `${name}@${version}[:patch:<hex>]:${source?}[:content:<hex>]:${integrity}`.
/// Source-backed packages fold in their stable specifier so two local
/// or git dependencies with the same manifest version don't collapse
/// onto the same graph hash when they point at different bytes.
///
/// `content` is the materialized-content fingerprint (see
/// [`ContentHashFn`]). It disambiguates source-backed deps that share a
/// coordinate but not bytes — e.g. a git dep whose `prepare` ran versus
/// the same commit installed under `--ignore-scripts`.
fn full_pkg_id(pkg: &LockedPackage, patch_hash: PatchHashFn<'_>, content: Option<&str>) -> String {
    let integrity = pkg.integrity.as_deref().unwrap_or("<no-integrity>");
    let source = pkg
        .local_source
        .as_ref()
        .map(|source| format!(":source:{}", source.specifier()))
        .unwrap_or_default();
    let content = content
        .map(|hex| format!(":content:{hex}"))
        .unwrap_or_default();
    match patch_hash(&pkg.name, &pkg.version) {
        Some(hex) => format!(
            "{}@{}:patch:{hex}{source}{content}:{integrity}",
            pkg.name, pkg.version
        ),
        None => format!("{}@{}{source}{content}:{integrity}", pkg.name, pkg.version),
    }
}

/// BLAKE3 over a canonical JSON serialization. `serde_json` plus
/// `BTreeMap` gives alphabetized keys; primitives serialize
/// deterministically. Return the full hex digest so callers can pick
/// whatever prefix length they want.
fn hash_canonical<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_vec(value).expect("graph hash input must serialize");
    blake3::hash(&json).to_hex().to_string()
}

#[derive(Serialize)]
struct NodeHashInput<'a> {
    engine: Option<&'a str>,
    deps: &'a str,
}

#[derive(Serialize)]
struct DepsHashInput<'a> {
    id: &'a str,
    deps: &'a BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirectDep, LocalSource, LockedPackage, LockfileGraph};
    use std::path::PathBuf;

    fn mk_pkg(name: &str, ver: &str, integrity: Option<&str>) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: ver.into(),
            integrity: integrity.map(str::to_string),
            dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            dep_path: format!("{name}@{ver}"),
            ..Default::default()
        }
    }

    fn empty_graph() -> LockfileGraph {
        let mut importers = BTreeMap::new();
        importers.insert(".".into(), Vec::<DirectDep>::new());
        LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn hash_is_deterministic_across_runs() {
        let mut g = empty_graph();
        g.packages.insert(
            "foo@1.0.0".into(),
            mk_pkg("foo", "1.0.0", Some("sha512-ABC")),
        );
        let h1 = compute_graph_hashes(&g, &|_| false, None);
        let h2 = compute_graph_hashes(&g, &|_| false, None);
        assert_eq!(h1.node_hash, h2.node_hash);
    }

    #[test]
    fn different_integrity_produces_different_hash() {
        let mut g1 = empty_graph();
        g1.packages
            .insert("foo@1.0.0".into(), mk_pkg("foo", "1.0.0", Some("sha512-A")));
        let mut g2 = empty_graph();
        g2.packages
            .insert("foo@1.0.0".into(), mk_pkg("foo", "1.0.0", Some("sha512-B")));
        let h1 = compute_graph_hashes(&g1, &|_| false, None);
        let h2 = compute_graph_hashes(&g2, &|_| false, None);
        assert_ne!(h1.node_hash["foo@1.0.0"], h2.node_hash["foo@1.0.0"]);
    }

    #[test]
    fn child_change_cascades_to_parent() {
        let mut g1 = empty_graph();
        g1.packages
            .insert("foo@1.0.0".into(), mk_pkg("foo", "1.0.0", Some("sha512-F")));
        let mut foo = mk_pkg("foo", "1.0.0", Some("sha512-F"));
        foo.dependencies.insert("bar".into(), "1.0.0".into());
        g1.packages.insert("foo@1.0.0".into(), foo);
        g1.packages.insert(
            "bar@1.0.0".into(),
            mk_pkg("bar", "1.0.0", Some("sha512-B1")),
        );

        let mut g2 = g1.clone();
        g2.packages.insert(
            "bar@1.0.0".into(),
            mk_pkg("bar", "1.0.0", Some("sha512-B2")),
        );

        let h1 = compute_graph_hashes(&g1, &|_| false, None);
        let h2 = compute_graph_hashes(&g2, &|_| false, None);
        assert_ne!(h1.node_hash["foo@1.0.0"], h2.node_hash["foo@1.0.0"]);
        assert_ne!(h1.node_hash["bar@1.0.0"], h2.node_hash["bar@1.0.0"]);
    }

    #[test]
    fn source_change_cascades_to_parent() {
        let mut g1 = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent
            .dependencies
            .insert("child".into(), "file+aaa".into());
        g1.packages.insert("parent@1.0.0".into(), parent);
        let mut child = mk_pkg("child", "1.0.0", None);
        child.dep_path = "child@file+aaa".into();
        child.local_source = Some(LocalSource::Directory(PathBuf::from("vendor/a")));
        g1.packages.insert("child@file+aaa".into(), child);

        let mut g2 = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent
            .dependencies
            .insert("child".into(), "file+bbb".into());
        g2.packages.insert("parent@1.0.0".into(), parent);
        let mut child = mk_pkg("child", "1.0.0", None);
        child.dep_path = "child@file+bbb".into();
        child.local_source = Some(LocalSource::Directory(PathBuf::from("vendor/b")));
        g2.packages.insert("child@file+bbb".into(), child);

        let h1 = compute_graph_hashes(&g1, &|_| false, None);
        let h2 = compute_graph_hashes(&g2, &|_| false, None);

        assert_ne!(
            h1.node_hash["child@file+aaa"],
            h2.node_hash["child@file+bbb"]
        );
        assert_ne!(h1.node_hash["parent@1.0.0"], h2.node_hash["parent@1.0.0"]);
    }

    #[test]
    fn engine_only_affects_packages_transitively_requiring_build() {
        let mut g = empty_graph();
        g.packages.insert(
            "pure@1.0.0".into(),
            mk_pkg("pure", "1.0.0", Some("sha512-P")),
        );
        g.packages.insert(
            "native@1.0.0".into(),
            mk_pkg("native", "1.0.0", Some("sha512-N")),
        );
        let mut consumer = mk_pkg("consumer", "1.0.0", Some("sha512-C"));
        consumer
            .dependencies
            .insert("native".into(), "1.0.0".into());
        g.packages.insert("consumer@1.0.0".into(), consumer);

        let allow_native = |pkg: &LockedPackage| pkg.registry_name() == "native";
        let engine_a = EngineName("linux-x64-node20".into());
        let engine_b = EngineName("linux-x64-node22".into());

        let h_a = compute_graph_hashes(&g, &allow_native, Some(&engine_a));
        let h_b = compute_graph_hashes(&g, &allow_native, Some(&engine_b));

        // `native` builds → engine-sensitive → different per engine
        assert_ne!(h_a.node_hash["native@1.0.0"], h_b.node_hash["native@1.0.0"]);
        // `consumer` depends on native → engine-sensitive
        assert_ne!(
            h_a.node_hash["consumer@1.0.0"],
            h_b.node_hash["consumer@1.0.0"]
        );
        // `pure` has no build in its subtree → engine-agnostic → stable
        assert_eq!(h_a.node_hash["pure@1.0.0"], h_b.node_hash["pure@1.0.0"]);
    }

    #[test]
    fn content_hash_disambiguates_same_coordinate() {
        // A git dep with no integrity: two installs share the same
        // `(name, version, source)` coordinate but materialize
        // different trees (prepare ran vs `--ignore-scripts`). Folding
        // the content fingerprint in must split them onto distinct
        // hashes; an absent fingerprint must leave the hash unchanged.
        let mut g = empty_graph();
        let mut pkg = mk_pkg("gitdep", "1.0.0", None);
        pkg.dep_path = "gitdep@git+abc".into();
        pkg.local_source = Some(LocalSource::Directory(PathBuf::from("clone")));
        g.packages.insert("gitdep@git+abc".into(), pkg);

        let none = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|_| None);
        let prepared = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == "gitdep@git+abc").then(|| "prepared".to_string())
        });
        let raw = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == "gitdep@git+abc").then(|| "raw".to_string())
        });

        assert_ne!(
            prepared.node_hash["gitdep@git+abc"], raw.node_hash["gitdep@git+abc"],
            "different content fingerprints must produce different hashes"
        );
        assert_ne!(
            none.node_hash["gitdep@git+abc"], prepared.node_hash["gitdep@git+abc"],
            "folding in a fingerprint must change the hash vs none"
        );
        // A no-op content fn reproduces the with-patches result exactly,
        // so existing GVS paths for the common case stay stable.
        let with_patches = compute_graph_hashes_with_patches(&g, &|_| false, None, &|_, _| None);
        assert_eq!(none.node_hash, with_patches.node_hash);
    }

    #[test]
    fn content_hash_cascades_to_parent() {
        // A parent that depends on the fingerprinted git dep must also
        // get a fresh hash, so its sibling symlink lands on the dep's
        // content-disambiguated path rather than dangling.
        let mut g = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent
            .dependencies
            .insert("gitdep".into(), "git+abc".into());
        g.packages.insert("parent@1.0.0".into(), parent);
        let mut child = mk_pkg("gitdep", "1.0.0", None);
        child.dep_path = "gitdep@git+abc".into();
        child.local_source = Some(LocalSource::Directory(PathBuf::from("clone")));
        g.packages.insert("gitdep@git+abc".into(), child);

        let a = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == "gitdep@git+abc").then(|| "prepared".to_string())
        });
        let b = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == "gitdep@git+abc").then(|| "raw".to_string())
        });
        assert_ne!(a.node_hash["parent@1.0.0"], b.node_hash["parent@1.0.0"]);
    }

    const URL_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn url_shaped_git_child_content_cascades_to_parent() {
        // Real pnpm lockfiles record a git dependency by its *resolved
        // URL* in the parent's `dependencies:` map, while the package is
        // keyed under the hashed `name@git+<hash>` form. The hasher must
        // canonicalize that URL-shaped value — a raw `name@<url>` lookup
        // misses the child, so its content fingerprint never reaches the
        // parent and two materially different trees collide on one GVS
        // path. (Distinct from `content_hash_cascades_to_parent`, which
        // feeds the already-canonical synthetic `git+abc` value.)
        let url = format!("https://github.com/request/request.git#{URL_SHA}");
        let child_key = shared_local_dep_path("request", &url).expect("git url is shareable");
        assert!(
            child_key.starts_with("request@git+"),
            "unexpected: {child_key}"
        );

        let mut g = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent.dependencies.insert("request".into(), url);
        g.packages.insert("parent@1.0.0".into(), parent);
        let mut child = mk_pkg("request", "2.88.0", None);
        child.dep_path = child_key.clone();
        child.local_source = Some(LocalSource::Directory(PathBuf::from("clone")));
        g.packages.insert(child_key.clone(), child);

        let prepared = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == child_key.as_str()).then(|| "prepared".to_string())
        });
        let raw = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == child_key.as_str()).then(|| "raw".to_string())
        });
        assert_ne!(
            prepared.node_hash["parent@1.0.0"], raw.node_hash["parent@1.0.0"],
            "URL-shaped git child fingerprint must cascade into the parent hash"
        );
    }

    #[test]
    fn url_shaped_tarball_child_content_cascades_to_parent() {
        // The codeload-archive form pnpm records for a `github:` dep that
        // resolves to a tarball. Keyed under `name@url+<hash>`; the raw
        // `name@<url>` lookup would skip it just like the git case.
        let url = format!("https://codeload.github.com/request/request/tar.gz/{URL_SHA}");
        let child_key = shared_local_dep_path("request", &url).expect("tarball url is shareable");
        assert!(
            child_key.starts_with("request@url+"),
            "unexpected: {child_key}"
        );

        let mut g = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent.dependencies.insert("request".into(), url);
        g.packages.insert("parent@1.0.0".into(), parent);
        let mut child = mk_pkg("request", "2.88.0", None);
        child.dep_path = child_key.clone();
        child.local_source = Some(LocalSource::Directory(PathBuf::from("clone")));
        g.packages.insert(child_key.clone(), child);

        let prepared = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == child_key.as_str()).then(|| "prepared".to_string())
        });
        let raw = compute_graph_hashes_full(&g, &|_| false, None, &|_, _| None, &|dp| {
            (dp == child_key.as_str()).then(|| "raw".to_string())
        });
        assert_ne!(
            prepared.node_hash["parent@1.0.0"], raw.node_hash["parent@1.0.0"],
            "URL-shaped tarball child fingerprint must cascade into the parent hash"
        );
    }

    #[test]
    fn url_shaped_git_child_engine_taint_cascades_to_parent() {
        // An allowlisted (building) git child recorded by URL must make
        // the parent engine-sensitive too; otherwise a parent installed
        // under a different engine reuses a GVS path built for the wrong
        // ABI. Requires the same canonical child lookup in
        // `transitively_requires_build`.
        let url = format!("https://github.com/request/request.git#{URL_SHA}");
        let child_key = shared_local_dep_path("request", &url).expect("git url is shareable");

        let mut g = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent.dependencies.insert("request".into(), url);
        g.packages.insert("parent@1.0.0".into(), parent);
        let mut child = mk_pkg("request", "2.88.0", None);
        child.dep_path = child_key.clone();
        child.local_source = Some(LocalSource::Directory(PathBuf::from("clone")));
        g.packages.insert(child_key, child);

        let allow_request = |pkg: &LockedPackage| pkg.registry_name() == "request";
        let engine_a = EngineName("linux-x64-node20".into());
        let engine_b = EngineName("linux-x64-node22".into());
        let h_a = compute_graph_hashes(&g, &allow_request, Some(&engine_a));
        let h_b = compute_graph_hashes(&g, &allow_request, Some(&engine_b));
        assert_ne!(
            h_a.node_hash["parent@1.0.0"], h_b.node_hash["parent@1.0.0"],
            "URL-shaped building git child must make the parent engine-sensitive"
        );
    }

    #[test]
    fn cycles_do_not_panic() {
        let mut g = empty_graph();
        let mut a = mk_pkg("a", "1.0.0", Some("sha512-A"));
        a.dependencies.insert("b".into(), "1.0.0".into());
        let mut b = mk_pkg("b", "1.0.0", Some("sha512-B"));
        b.dependencies.insert("a".into(), "1.0.0".into());
        g.packages.insert("a@1.0.0".into(), a);
        g.packages.insert("b@1.0.0".into(), b);

        let h = compute_graph_hashes(&g, &|_| false, None);
        assert!(h.node_hash.contains_key("a@1.0.0"));
        assert!(h.node_hash.contains_key("b@1.0.0"));
    }

    fn shareable_source() -> LocalSource {
        LocalSource::RemoteTarball(crate::RemoteTarballSource {
            url: "https://example.com/dep.tgz".into(),
            integrity: "sha512-Z".into(),
            git_hosted: false,
        })
    }

    #[test]
    fn content_affected_covers_shareable_source_and_all_ancestors() {
        // parent -> midware -> tarball(shareable); `pure` is an unrelated
        // sibling whose subtree contains no source dep.
        let mut g = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent.dependencies.insert("midware".into(), "1.0.0".into());
        g.packages.insert("parent@1.0.0".into(), parent);

        let mut midware = mk_pkg("midware", "1.0.0", Some("sha512-M"));
        midware
            .dependencies
            .insert("tardep".into(), "url+aaa".into());
        g.packages.insert("midware@1.0.0".into(), midware);

        let mut tardep = mk_pkg("tardep", "1.0.0", None);
        tardep.dep_path = "tardep@url+aaa".into();
        tardep.local_source = Some(shareable_source());
        g.packages.insert("tardep@url+aaa".into(), tardep);

        g.packages.insert(
            "pure@1.0.0".into(),
            mk_pkg("pure", "1.0.0", Some("sha512-X")),
        );

        let affected = content_affected_dep_paths(&g);
        assert!(
            affected.contains("tardep@url+aaa"),
            "the source leaf itself"
        );
        assert!(affected.contains("midware@1.0.0"), "direct ancestor");
        assert!(affected.contains("parent@1.0.0"), "transitive ancestor");
        assert!(
            !affected.contains("pure@1.0.0"),
            "a source-free subtree must stay prewarm-eligible"
        );
    }

    #[test]
    fn content_affected_handles_self_referential_cycle() {
        // host <-> srcdep(shareable) cycle, mirroring a real-world
        // self-referential peer cycle. Both nodes must be flagged
        // regardless of the back-edge; reverse reachability from the
        // source seed makes this order-independent.
        let mut g = empty_graph();
        let mut host = mk_pkg("host", "2.0.0", Some("sha512-L"));
        host.dependencies.insert("srcdep".into(), "url+bbb".into());
        g.packages.insert("host@2.0.0".into(), host);

        let mut srcdep = mk_pkg("srcdep", "2.0.0", None);
        srcdep.dep_path = "srcdep@url+bbb".into();
        srcdep.local_source = Some(shareable_source());
        srcdep.dependencies.insert("host".into(), "2.0.0".into());
        g.packages.insert("srcdep@url+bbb".into(), srcdep);

        let affected = content_affected_dep_paths(&g);
        assert!(affected.contains("srcdep@url+bbb"));
        assert!(
            affected.contains("host@2.0.0"),
            "ancestor inside a cycle with the source must still be flagged"
        );
    }

    #[test]
    fn content_affected_ignores_non_shareable_local_sources() {
        // A `file:` directory dep is not globally shareable: it gets no
        // content fingerprint, so its hash is identical across prewarm
        // and link and prewarm may safely materialize its ancestors.
        let mut g = empty_graph();
        let mut parent = mk_pkg("parent", "1.0.0", Some("sha512-P"));
        parent.dependencies.insert("dir".into(), "file+ccc".into());
        g.packages.insert("parent@1.0.0".into(), parent);

        let mut dir = mk_pkg("dir", "1.0.0", None);
        dir.dep_path = "dir@file+ccc".into();
        dir.local_source = Some(LocalSource::Directory(PathBuf::from("vendor/dir")));
        g.packages.insert("dir@file+ccc".into(), dir);

        let affected = content_affected_dep_paths(&g);
        assert!(affected.is_empty(), "got: {affected:?}");
    }

    #[test]
    fn hashed_dep_path_appends_to_leaf() {
        let mut h = GraphHashes::default();
        h.node_hash.insert("foo@1.0.0".into(), "a".repeat(64));
        assert!(h.hashed_dep_path("foo@1.0.0").starts_with("foo@1.0.0-aa"));
    }

    #[test]
    fn hashed_dep_path_preserves_scope() {
        let mut h = GraphHashes::default();
        h.node_hash.insert("@swc/core@1.3.0".into(), "b".repeat(64));
        let got = h.hashed_dep_path("@swc/core@1.3.0");
        assert!(got.starts_with("@swc/core@1.3.0-bb"), "got: {got}");
        // Scope prefix survives unchanged so the existing directory
        // layout (`virtual_store/@scope/<leaf>`) still resolves.
        assert!(got.starts_with("@swc/"));
    }

    #[test]
    fn hashed_dep_path_falls_back_to_raw_when_absent() {
        let h = GraphHashes::default();
        assert_eq!(h.hashed_dep_path("foo@1.0.0"), "foo@1.0.0");
    }

    #[test]
    fn engine_name_parses_node_version() {
        let e = engine_name_default("v20.10.0");
        assert!(e.0.ends_with("-node20"));
        let e = engine_name_default("22.0.0");
        assert!(e.0.ends_with("-node22"));
    }

    #[test]
    fn node_arch_maps_to_node_conventions() {
        assert_eq!(node_arch("x86_64"), "x64");
        assert_eq!(node_arch("aarch64"), "arm64");
        assert_eq!(node_arch("x86"), "ia32");
        // Unknown architectures pass through rather than getting
        // silently remapped onto an adjacent bucket.
        assert_eq!(node_arch("riscv64"), "riscv64");
    }

    // --- graph_identity_hash (no-churn write guard primitive) ---

    use crate::DepType;

    fn graph_with_importer(pkgs: &[LockedPackage], direct: &[(&str, &str)]) -> LockfileGraph {
        let mut g = empty_graph();
        for p in pkgs {
            g.packages.insert(p.dep_path.clone(), p.clone());
        }
        let deps: Vec<DirectDep> = direct
            .iter()
            .map(|(name, dep_path)| DirectDep {
                name: (*name).to_string(),
                dep_path: (*dep_path).to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            })
            .collect();
        g.importers.insert(".".into(), deps);
        g
    }

    #[test]
    fn graph_identity_hash_equal_for_identical_graphs() {
        let g1 = graph_with_importer(
            &[mk_pkg("foo", "1.0.0", Some("sha512-A"))],
            &[("foo", "foo@1.0.0")],
        );
        let g2 = graph_with_importer(
            &[mk_pkg("foo", "1.0.0", Some("sha512-A"))],
            &[("foo", "foo@1.0.0")],
        );
        assert_eq!(
            graph_identity_hash(&g1, &|_| false),
            graph_identity_hash(&g2, &|_| false)
        );
    }

    #[test]
    fn graph_identity_hash_differs_when_a_package_changes() {
        let g1 = graph_with_importer(
            &[mk_pkg("foo", "1.0.0", Some("sha512-A"))],
            &[("foo", "foo@1.0.0")],
        );
        let g2 = graph_with_importer(
            &[mk_pkg("foo", "1.0.0", Some("sha512-B"))],
            &[("foo", "foo@1.0.0")],
        );
        assert_ne!(
            graph_identity_hash(&g1, &|_| false),
            graph_identity_hash(&g2, &|_| false)
        );
    }

    #[test]
    fn graph_identity_hash_differs_when_importer_direct_dep_changes() {
        // Same package set, different importer edge (a dep was added) —
        // the lockfile genuinely changed, so the digest must move.
        let pkgs = [mk_pkg("foo", "1.0.0", Some("sha512-A"))];
        let g1 = graph_with_importer(&pkgs, &[]);
        let g2 = graph_with_importer(&pkgs, &[("foo", "foo@1.0.0")]);
        assert_ne!(
            graph_identity_hash(&g1, &|_| false),
            graph_identity_hash(&g2, &|_| false)
        );
    }

    #[test]
    fn graph_identity_hash_is_engine_agnostic() {
        // A package allowed to build would taint per-host engine in the
        // per-node hash, but the identity hash forces `engine: None`, so
        // the digest is the same regardless of the build policy passed.
        let g = graph_with_importer(
            &[mk_pkg("native", "1.0.0", Some("sha512-N"))],
            &[("native", "native@1.0.0")],
        );
        let allow_all = |_: &LockedPackage| true;
        let allow_none = |_: &LockedPackage| false;
        assert_eq!(
            graph_identity_hash(&g, &allow_all),
            graph_identity_hash(&g, &allow_none)
        );
    }

    #[test]
    fn graph_identity_hash_stable_under_importer_edge_reorder() {
        // Re-parsing a foreign lockfile can yield direct deps in a
        // different order; the digest must not depend on edge order.
        let pkgs = [
            mk_pkg("a", "1.0.0", Some("sha512-A")),
            mk_pkg("b", "1.0.0", Some("sha512-B")),
        ];
        let g1 = graph_with_importer(&pkgs, &[("a", "a@1.0.0"), ("b", "b@1.0.0")]);
        let g2 = graph_with_importer(&pkgs, &[("b", "b@1.0.0"), ("a", "a@1.0.0")]);
        assert_eq!(
            graph_identity_hash(&g1, &|_| false),
            graph_identity_hash(&g2, &|_| false)
        );
    }
}
