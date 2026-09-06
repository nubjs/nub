//! Platform filtering for `os` / `cpu` / `libc` package metadata.
//!
//! npm-style packages can declare the platforms they support via the
//! `os`, `cpu`, and `libc` arrays in `package.json`. Each entry is
//! either a positive match (`"linux"`, `"x64"`, `"glibc"`) or a
//! negation prefixed with `!` (`"!win32"`). pnpm's rule:
//!
//!   - empty array        → unconstrained (installable everywhere)
//!   - any negation hit   → reject
//!   - at least one pos   → accept only if one positive matches
//!   - negations only     → accept if no negation matched
//!
//! pnpm lets the user widen the match set beyond the host via
//! `pnpm.supportedArchitectures` — an object with `os`/`cpu`/`libc`
//! arrays, each entry either a concrete value or the literal `"current"`
//! which expands to the host triple. The package passes if ANY of the
//! (os, cpu, libc) combinations in the supported set is installable.
//!
//! This module stays intentionally small: no reading of config, no
//! serde, just the matcher and host detection. Configuration lives on
//! the `Resolver`, which calls [`is_supported`] during filtering.

/// User-declared override for the host triple used when filtering
/// optional dependencies. Missing arrays fall back to the host; the
/// literal `"current"` inside any array expands to the same host value
/// so users can write `["current", "linux"]` to keep their native
/// platform *and* also resolve optionals for Linux.
#[derive(Debug, Clone, Default)]
pub struct SupportedArchitectures {
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
    /// When true, [`is_supported`] accepts every package regardless of
    /// its `os`/`cpu`/`libc`. Set at *resolve* time for the committed,
    /// cross-platform lockfiles (pnpm-lock.yaml, aube-lock.yaml,
    /// bun.lock) so every optional-dep variant a package declares lands
    /// in the lockfile — exactly what pnpm and bun both record,
    /// regardless of the host running the resolve. Link-time filtering
    /// (`filter_graph`) and the streaming-fetch gate run against the
    /// host triple instead, so `node_modules` and the tarball downloads
    /// stay trimmed to the host.
    pub accept_all: bool,
}

impl SupportedArchitectures {
    /// Expand any `"current"` entries to the host triple and default
    /// empty arrays to `[host]`. The result is a non-empty list of
    /// (os, cpu, libc) combinations the caller can test against.
    fn combinations(&self) -> Vec<(String, String, String)> {
        let host = host_triple();
        let expand = |field: &[String], host_val: &str| -> Vec<String> {
            if field.is_empty() {
                return vec![host_val.to_string()];
            }
            field
                .iter()
                .map(|v| {
                    if v == "current" {
                        host_val.to_string()
                    } else {
                        v.clone()
                    }
                })
                .collect()
        };
        let os = expand(&self.os, host.0);
        let cpu = expand(&self.cpu, host.1);
        let libc = expand(&self.libc, host.2);
        let mut out = Vec::with_capacity(os.len() * cpu.len() * libc.len());
        for o in &os {
            for c in &cpu {
                for l in &libc {
                    out.push((o.clone(), c.clone(), l.clone()));
                }
            }
        }
        out
    }
}

/// Return the host's (os, cpu, libc) triple using npm's vocabulary.
/// `libc` is `"glibc"` / `"musl"` on Linux and `""` elsewhere — npm
/// only sets `libc` on Linux packages, so non-Linux hosts treat libc
/// constraints as a no-op.
pub fn host_triple() -> (&'static str, &'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let cpu = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        other => other,
    };
    // Detect libc at runtime, not compile time. Old code used
    // `cfg!(target_env = "musl")` which is the toolchain that built
    // the aube binary, not the host's libc. Real bug: an aube static
    // binary built against musl and shipped to glibc users reported
    // libc=musl everywhere, and the glibc-built distro reported
    // glibc everywhere. Wrong prebuilts got installed, runtime
    // ld.so errors. Probe /lib/ld-musl-* vs /lib*/ld-linux-*.
    let libc = if std::env::consts::OS == "linux" {
        detect_linux_libc()
    } else {
        ""
    };
    (os, cpu, libc)
}

/// Probe the active dynamic linker to tell musl from glibc at runtime.
/// Authoritative signal is `/proc/self/maps`: the dynamic linker that
/// loaded the running aube binary is always mmap'd into the process,
/// so whichever of `ld-musl-*` or `ld-linux-*` shows up there is the
/// libc the host actually runs. Cached once via OnceLock.
///
/// The previous /lib-scan heuristic broke on Ubuntu glibc hosts that
/// `apt install musl` for cross-compile tooling: the musl package
/// drops `/lib/ld-musl-<arch>.so.1` alongside the system glibc loader,
/// and a first-match scan returned "musl", causing aube to install
/// `*-linux-x64-musl` native bindings that node (linked against
/// glibc) cannot load. /proc/self/maps cuts straight to which loader
/// actually runs and ignores the partial-install noise. The /lib
/// fallback is kept for non-Linux containers / stripped rootfs that
/// expose no procfs, but checks glibc *first* so a dual-loader system
/// still resolves correctly there.
fn detect_linux_libc() -> &'static str {
    use std::sync::OnceLock;
    static CACHE: OnceLock<&'static str> = OnceLock::new();
    CACHE.get_or_init(|| {
        if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
            if maps.contains("/ld-musl-") {
                return "musl";
            }
            if maps.contains("/ld-linux") {
                return "glibc";
            }
        }
        let glibc_dirs = [
            "/lib",
            "/lib64",
            "/lib/x86_64-linux-gnu",
            "/lib/aarch64-linux-gnu",
        ];
        for dir in glibc_dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("ld-linux") {
                        return "glibc";
                    }
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir("/lib") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("ld-musl-") {
                    return "musl";
                }
            }
        }
        "glibc"
    })
}

/// Apply npm's `os`/`cpu`/`libc` rules to a single (pkg_field, host)
/// pair. An empty pkg array is unconstrained; negations reject; at
/// least one positive entry means one must match.
///
/// `host` is the value being selected FOR — the host's own triple, or an
/// entry the caller configured. The literal `*` there accepts any package
/// value on this axis, which is what `--os='*'` selects. pnpm has no such
/// spelling (it silently installs nothing), so this is additive: no config
/// that works under pnpm changes meaning, and the shape users reach for
/// first now does what it looks like.
fn field_matches(pkg_field: &[String], host: &str) -> bool {
    if pkg_field.is_empty() || host == "*" {
        return true;
    }
    let mut has_positive = false;
    let mut positive_matched = false;
    for entry in pkg_field {
        if let Some(neg) = entry.strip_prefix('!') {
            if neg == host {
                return false;
            }
        } else {
            has_positive = true;
            if entry == host {
                positive_matched = true;
            }
        }
    }
    !has_positive || positive_matched
}

/// Decide whether a package is installable on any of the (os, cpu,
/// libc) combinations expanded from `supported`. The `pkg_libc` check
/// is skipped when the host libc is empty (non-Linux) — npm doesn't
/// enforce libc off Linux.
pub fn is_supported(
    pkg_os: &[String],
    pkg_cpu: &[String],
    pkg_libc: &[String],
    supported: &SupportedArchitectures,
) -> bool {
    // pnpm-lock parity: record every declared variant in the lockfile
    // regardless of host. Host-only trimming happens later via
    // `filter_graph` / the streaming-fetch gate, which use the real host
    // triple rather than this accept-all set.
    if supported.accept_all {
        return true;
    }
    for (os, cpu, libc) in supported.combinations() {
        if !field_matches(pkg_os, &os) {
            continue;
        }
        if !field_matches(pkg_cpu, &cpu) {
            continue;
        }
        if !libc.is_empty() && !field_matches(pkg_libc, &libc) {
            continue;
        }
        return true;
    }
    false
}

/// Remove optional dependencies that fail the platform check or appear in the
/// ignore list from a parsed `LockfileGraph`, then garbage-collect any packages
/// that become unreachable from the surviving importers.
///
/// Used by the install-from-lockfile path, where the resolver's inline
/// filter never runs: the lockfile carries os/cpu/libc per package so
/// aube can re-check on every platform without reparsing packuments.
///
/// Root and transitive optional edges are inspected directly. Any package that
/// becomes unreachable after optional-edge pruning is removed by the GC pass.
pub fn filter_graph(
    graph: &mut aube_lockfile::LockfileGraph,
    supported: &SupportedArchitectures,
    ignored: &std::collections::BTreeSet<String>,
) {
    use crate::FxHashSet;
    use aube_lockfile::DepType;

    let is_mismatched =
        |pkg: &aube_lockfile::LockedPackage| !is_supported(&pkg.os, &pkg.cpu, &pkg.libc, supported);

    // 1. Drop root optional deps by name or by platform.
    for deps in graph.importers.values_mut() {
        deps.retain(|dep| {
            if dep.dep_type != DepType::Optional {
                return true;
            }
            if ignored.contains(&dep.name) {
                return false;
            }
            !matches!(graph.packages.get(&dep.dep_path), Some(pkg) if is_mismatched(pkg))
        });
    }

    // 2. Drop transitive optional deps by name or platform. The pnpm parser
    // mirrors active optional edges into `dependencies`, so remove that edge
    // whenever the optional edge is filtered.
    let package_keys: FxHashSet<String> = graph.packages.keys().cloned().collect();
    let mismatched_packages: FxHashSet<String> = graph
        .packages
        .iter()
        .filter(|(_, pkg)| is_mismatched(pkg))
        .map(|(dep_path, _)| dep_path.clone())
        .collect();
    for pkg in graph.packages.values_mut() {
        let mut removed = Vec::new();
        pkg.optional_dependencies.retain(|name, tail| {
            // Resolve through every reader convention (incl. the
            // git/remote-tarball `name@url+<hash>` form) so a
            // platform-mismatched optional git/tarball child is actually
            // pruned here rather than surviving until the GC pass below.
            let child_is_mismatched =
                match aube_lockfile::resolve_dep_edge(name, tail, |k| package_keys.contains(k)) {
                    Some(child_key) => mismatched_packages.contains(&child_key),
                    None => false,
                };
            let keep = !ignored.contains(name) && !child_is_mismatched;
            if !keep {
                removed.push(name.clone());
            }
            keep
        });
        for name in removed {
            pkg.dependencies.remove(&name);
        }
    }

    // 2b. Sever `bundleDependencies` edges. A bundled dep ships *inside*
    //     its parent's tarball along with that dep's entire closure, so
    //     it is materialized from the parent's extracted tree — never
    //     fetched or linked as a standalone node (this is exactly what
    //     npm records `inBundle` for). But npm's lockfile still
    //     enumerates the bundled subtree with live `dependencies` edges,
    //     so without cutting the parent→bundle edge the GC walk below
    //     keeps the whole closure and the fetch/link path promotes it to
    //     first-class nodes: redundant standalone copies online, and a
    //     fatal `--offline` miss on any bundled child not in cache.
    //     Cutting the edge per the parent's own `bundled_dependencies`
    //     is context-correct (a legitimate non-bundled consumer of the
    //     same name keeps its edge) and lets the GC prune the now-
    //     unreachable closure — the same shape as the optional-edge
    //     pruning above. The fresh-resolve path already skips these via
    //     the packument's `bundledDependencies`; this is its frozen-
    //     restore counterpart. (v1 lockfiles carry no parent-level
    //     `bundleDependencies`; the npm reader reconstructs it from the
    //     nested `bundled:true` children so this edge cut fires there
    //     too.)
    for pkg in graph.packages.values_mut() {
        if pkg.bundled_dependencies.is_empty() {
            continue;
        }
        let bundled = pkg.bundled_dependencies.clone();
        for name in &bundled {
            pkg.dependencies.remove(name);
            pkg.optional_dependencies.remove(name);
        }
    }

    // 3. Garbage-collect unreachable packages by walking from the
    //    surviving roots.
    let mut reachable: FxHashSet<String> = FxHashSet::default();
    let mut stack: Vec<String> = Vec::new();
    for deps in graph.importers.values() {
        for dep in deps {
            stack.push(dep.dep_path.clone());
        }
    }
    while let Some(dep_path) = stack.pop() {
        if !reachable.insert(dep_path.clone()) {
            continue;
        }
        if let Some(pkg) = graph.packages.get(&dep_path) {
            // pnpm mirrors active optional edges into `dependencies`, but
            // Yarn Berry records them only in `optional_dependencies`.
            for (name, tail) in pkg
                .dependencies
                .iter()
                .chain(pkg.optional_dependencies.iter())
            {
                // Resolve the edge through every reader convention,
                // including the git/remote-tarball `name@url+<hash>` form
                // — otherwise a canonically-keyed git/tarball child (and
                // its whole subtree) is unreachable here and gets GC'd.
                if let Some(child) =
                    aube_lockfile::resolve_dep_edge(name, tail, |k| graph.packages.contains_key(k))
                {
                    stack.push(child);
                }
            }
        }
    }
    graph.packages.retain(|k, _| reachable.contains(k));
}

/// Set each package's `optional` flag the way pnpm marks the
/// `snapshots:` section: a package is `optional: true` when it is
/// reachable *only* through optional dependency edges (the classic case
/// is every `@esbuild/*` platform native sitting under `esbuild`'s
/// `optionalDependencies`). pnpm derives this during resolution; aube
/// recomputes it as a post-resolve pass so freshly resolved lockfiles
/// carry the same markers pnpm writes instead of an empty `{}` snapshot.
pub fn mark_optional_packages(graph: &mut aube_lockfile::LockfileGraph) {
    let optional = optional_only_packages(graph);
    for (dep_path, pkg) in graph.packages.iter_mut() {
        pkg.optional = optional.contains(dep_path);
    }
}

/// The dep_paths [`mark_optional_packages`] would mark `optional: true`,
/// derived from the graph's own edges without mutating it.
///
/// Exists because `LockedPackage::optional` is only populated on two paths —
/// the pnpm reader and the fresh-resolve pass above — so a frozen install off
/// an npm / bun / yarn lockfile carries `optional: false` on every package even
/// though the graph still describes the optional edges. A consumer whose
/// behavior must not silently vary by incumbent lockfile format asks here
/// instead of reading the field.
///
/// Algorithm: seed a `required` set from every non-optional direct dependency of
/// every importer, then walk each required package's *non-optional* edges. A
/// package's non-optional edges are its `dependencies` minus its
/// `optional_dependencies`, because the pnpm parser mirrors active optional
/// edges into `dependencies`. Any package not reached this way is optional. A
/// single fully-required path keeps a package required even when other paths to
/// it are optional, matching pnpm and npm (whose `calcDepFlags` states the same
/// invariant: "a node still flagged optional must only be reachable via optional
/// edges").
pub fn optional_only_packages(
    graph: &aube_lockfile::LockfileGraph,
) -> aube_util::collections::FxSet<String> {
    use crate::FxHashSet;
    use aube_lockfile::DepType;

    let mut required: FxHashSet<String> = FxHashSet::default();
    let mut stack: Vec<String> = Vec::new();
    for deps in graph.importers.values() {
        for dep in deps {
            if dep.dep_type != DepType::Optional {
                stack.push(dep.dep_path.clone());
            }
        }
    }
    while let Some(dep_path) = stack.pop() {
        if !required.insert(dep_path.clone()) {
            continue;
        }
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        for (name, tail) in &pkg.dependencies {
            // Skip optional edges. `dependencies` carries pnpm's mirrored
            // active optionals, so the `optional_dependencies` membership
            // check is what separates a required edge from an optional one.
            if pkg.optional_dependencies.contains_key(name) {
                continue;
            }
            // Match `filter_graph`'s child-key convention (incl. the
            // git/remote-tarball `name@url+<hash>` form) so a required
            // git/tarball dep isn't mis-marked optional-only.
            if let Some(child) =
                aube_lockfile::resolve_dep_edge(name, tail, |k| graph.packages.contains_key(k))
            {
                stack.push(child);
            }
        }
    }
    graph
        .packages
        .keys()
        .filter(|dep_path| !required.contains(*dep_path))
        .cloned()
        .collect()
}

/// Populate each package's `transitive_peer_dependencies` the way pnpm
/// does: a snapshot lists every peer name that some package in its
/// dependency subtree declares but leaves unresolved (the peers that
/// "bubble up" to be provided by a consumer). A peer that *was* resolved
/// is mirrored into the declaring package's `dependencies` (pnpm and aube
/// both do this — e.g. `@babel/core` lands in
/// `@babel/helper-module-transforms`'s deps), so `peer_dependencies` minus
/// `dependencies` is exactly the unresolved set. Those unresolved names are
/// propagated to every ancestor; a package never lists its own peers.
///
/// Runs on the final, peer-contextualized graph (after `apply_peer_contexts`
/// and the dedupe passes) so dep-path tails carry their peer suffixes.
pub fn mark_transitive_peer_dependencies(graph: &mut aube_lockfile::LockfileGraph) {
    use crate::{FxHashMap, FxHashSet};
    use std::collections::BTreeSet;

    // Reverse edges (child dep_path -> the parents that depend on it) plus
    // each package's unresolved declared peers.
    let mut parents: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut unresolved: FxHashMap<String, Vec<String>> = FxHashMap::default();

    for (dep_path, pkg) in &graph.packages {
        for (name, tail) in pkg
            .dependencies
            .iter()
            .chain(pkg.optional_dependencies.iter())
        {
            // Skip resolved-peer edges. A dependency the package also
            // declares as a peer (e.g. `eslint` inside an eslint plugin) is
            // an injected peer, not an owned dependency — pnpm satisfies it
            // from the consumer's context and does not bubble that peer's
            // own transitive peers through the edge. Mirroring that keeps a
            // plugin from inheriting `supports-color`/`typescript` purely
            // because its injected `eslint`/`typescript` peer transitively
            // depends on them.
            if pkg.peer_dependencies.contains_key(name)
                || pkg.peer_dependencies_meta.contains_key(name)
            {
                continue;
            }
            // Match `filter_graph`'s child-key convention (incl. the
            // git/remote-tarball `name@url+<hash>` form) so peers bubble
            // through git/tarball edges too.
            if let Some(child) =
                aube_lockfile::resolve_dep_edge(name, tail, |k| graph.packages.contains_key(k))
            {
                parents.entry(child).or_default().push(dep_path.clone());
            } else {
                // Edge points outside the resolved graph (workspace
                // `link:`/`file:` deps, or a child pruned by platform
                // filtering). It has no snapshot to bubble peers through,
                // so dropping it is correct — log at debug for anyone
                // chasing a missing `transitivePeerDependencies` entry.
                tracing::debug!(
                    parent = %dep_path,
                    dep = %name,
                    tail = %tail,
                    "transitive-peer pass: dependency edge has no graph node, skipping"
                );
            }
        }
        // Declared peers plus pnpm's meta-only peers (the optional
        // `peerDependenciesMeta` keys, folded in as `*` by the helper —
        // e.g. debug's `supports-color`). A resolved peer is mirrored into
        // `dependencies` (pnpm does the same for active optionals too, so
        // only `dependencies` needs checking — never `optional_dependencies`),
        // so subtracting `dependencies` keys leaves exactly the unresolved
        // set that bubbles up.
        let own: BTreeSet<String> = pkg
            .peer_dependencies_with_meta_defaults()
            .into_keys()
            .filter(|p| !pkg.dependencies.contains_key(p))
            .collect();
        if !own.is_empty() {
            unresolved.insert(dep_path.clone(), own.into_iter().collect());
        }
    }

    // Bubble each package's unresolved peers up to every ancestor. The
    // originating package is pre-marked visited, so it never collects its
    // own peers even inside a dependency cycle.
    let mut acc: FxHashMap<String, BTreeSet<String>> = FxHashMap::default();
    for (origin, peers) in &unresolved {
        let mut visited: FxHashSet<String> = FxHashSet::default();
        visited.insert(origin.clone());
        let mut stack: Vec<String> = parents.get(origin).cloned().unwrap_or_default();
        while let Some(node) = stack.pop() {
            if !visited.insert(node.clone()) {
                continue;
            }
            let entry = acc.entry(node.clone()).or_default();
            entry.extend(peers.iter().cloned());
            if let Some(ps) = parents.get(&node) {
                stack.extend(ps.iter().cloned());
            }
        }
    }

    for (dep_path, pkg) in graph.packages.iter_mut() {
        pkg.transitive_peer_dependencies = acc
            .get(dep_path)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn empty_fields_accept_any_host() {
        let sup = SupportedArchitectures::default();
        assert!(is_supported(&[], &[], &[], &sup));
    }

    #[test]
    fn positive_match_rules() {
        assert!(field_matches(&s(&["linux", "darwin"]), "linux"));
        assert!(!field_matches(&s(&["linux", "darwin"]), "win32"));
    }

    #[test]
    fn negation_rejects_match() {
        assert!(!field_matches(&s(&["!win32"]), "win32"));
        assert!(field_matches(&s(&["!win32"]), "linux"));
    }

    #[test]
    fn mixed_negation_and_positive() {
        // Negation takes precedence: even if a positive also matches,
        // hitting a negation rejects.
        assert!(!field_matches(&s(&["linux", "!linux"]), "linux"));
    }

    #[test]
    fn supported_architectures_widens_with_current() {
        // `["current", "linux"]` should accept the host *or* linux.
        let sup = SupportedArchitectures {
            os: s(&["current", "linux"]),
            ..Default::default()
        };
        // A linux-only package passes regardless of host.
        assert!(is_supported(&s(&["linux"]), &[], &[], &sup));
    }

    #[test]
    fn accept_all_accepts_every_arch_including_non_host_triples() {
        // pnpm/bun parity: `accept_all` records every optional-dep
        // variant a package declares, even triples a host-only filter
        // would reject (darwin-x64 on an arm64 mac, freebsd, ppc64,
        // s390x, …). Without it, a regenerated cross-platform lockfile
        // loses arches pnpm/bun keep, breaking teammates on those
        // platforms.
        let sup = SupportedArchitectures {
            accept_all: true,
            ..Default::default()
        };
        assert!(is_supported(&s(&["darwin"]), &s(&["x64"]), &[], &sup));
        assert!(is_supported(&s(&["freebsd"]), &s(&["arm64"]), &[], &sup));
        assert!(is_supported(
            &s(&["linux"]),
            &s(&["ppc64"]),
            &s(&["glibc"]),
            &sup
        ));
        assert!(is_supported(
            &s(&["openharmony"]),
            &s(&["arm64"]),
            &[],
            &sup
        ));
        assert!(is_supported(&s(&["win32"]), &s(&["ia32"]), &[], &sup));
        // Sanity: a host-only (default) set rejects at least one of
        // these, so the accept-all branch is doing real work.
        let host_only = SupportedArchitectures::default();
        let (host_os, _, _) = host_triple();
        if host_os != "freebsd" {
            assert!(!is_supported(
                &s(&["freebsd"]),
                &s(&["arm64"]),
                &[],
                &host_only
            ));
        }
    }

    #[test]
    fn filter_graph_prunes_transitive_optional_platform_mismatches() {
        let supported = SupportedArchitectures {
            os: s(&["darwin"]),
            cpu: s(&["arm64"]),
            ..Default::default()
        };
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "host".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        graph.packages.insert(
            "host@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "host".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dependencies: [
                    ("native-darwin".to_string(), "1.0.0".to_string()),
                    ("native-linux".to_string(), "1.0.0".to_string()),
                ]
                .into(),
                optional_dependencies: [
                    ("native-darwin".to_string(), "1.0.0".to_string()),
                    ("native-linux".to_string(), "1.0.0".to_string()),
                ]
                .into(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "native-darwin@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "native-darwin".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "native-darwin@1.0.0".to_string(),
                os: s(&["darwin"]).into(),
                cpu: s(&["arm64"]).into(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "native-linux@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "native-linux".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "native-linux@1.0.0".to_string(),
                os: s(&["linux"]).into(),
                cpu: s(&["x64"]).into(),
                ..Default::default()
            },
        );

        filter_graph(&mut graph, &supported, &Default::default());

        let host = graph.packages.get("host@1.0.0").unwrap();
        assert!(host.dependencies.contains_key("native-darwin"));
        assert!(!host.dependencies.contains_key("native-linux"));
        assert!(graph.packages.contains_key("native-darwin@1.0.0"));
        assert!(!graph.packages.contains_key("native-linux@1.0.0"));
    }

    #[test]
    fn filter_graph_keeps_supported_yarn_berry_optional_children() {
        let supported = SupportedArchitectures {
            os: s(&["darwin"]),
            cpu: s(&["arm64"]),
            ..Default::default()
        };
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "host".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        graph.packages.insert(
            "host@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "host".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "host@1.0.0".to_string(),
                // Yarn Berry does not mirror optional edges here.
                dependencies: Default::default(),
                optional_dependencies: [("native-darwin".to_string(), "1.0.0".to_string())].into(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "native-darwin@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "native-darwin".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "native-darwin@1.0.0".to_string(),
                os: s(&["darwin"]).into(),
                cpu: s(&["arm64"]).into(),
                ..Default::default()
            },
        );

        filter_graph(&mut graph, &supported, &Default::default());

        assert!(graph.packages.contains_key("native-darwin@1.0.0"));
    }

    fn dep(name: &str, dep_type: aube_lockfile::DepType) -> aube_lockfile::DirectDep {
        aube_lockfile::DirectDep {
            name: name.to_string(),
            dep_path: format!("{name}@1.0.0"),
            dep_type,
            specifier: Some("1.0.0".to_string()),
        }
    }

    fn pkg(name: &str, deps: &[&str], opt_deps: &[&str]) -> (String, aube_lockfile::LockedPackage) {
        let dep_path = format!("{name}@1.0.0");
        (
            dep_path.clone(),
            aube_lockfile::LockedPackage {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                dep_path,
                dependencies: deps
                    .iter()
                    .map(|d| ((*d).to_string(), "1.0.0".to_string()))
                    .collect(),
                optional_dependencies: opt_deps
                    .iter()
                    .map(|d| ((*d).to_string(), "1.0.0".to_string()))
                    .collect(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn mark_optional_packages_marks_optional_only_reachable() {
        use aube_lockfile::DepType;
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![
                dep("host", DepType::Production),
                dep("also-required", DepType::Production),
                dep("opt-root", DepType::Optional),
            ],
        );
        // `host` has a required prod dep (`shared`), two optional-only
        // natives, and `dual` reachable both optionally (here) and via a
        // required edge from `also-required`. pnpm mirrors active optionals
        // into `dependencies`, so they appear in both maps.
        graph.packages.extend([
            pkg(
                "host",
                &["shared", "native-darwin", "native-linux", "dual"],
                &["native-darwin", "native-linux", "dual"],
            ),
            pkg("also-required", &["dual"], &[]),
            pkg("shared", &[], &[]),
            pkg("native-darwin", &[], &[]),
            pkg("native-linux", &[], &[]),
            pkg("dual", &[], &[]),
            pkg("opt-root", &[], &[]),
        ]);

        mark_optional_packages(&mut graph);

        let is_opt = |k: &str| graph.packages[k].optional;
        // Required by a non-optional path.
        assert!(!is_opt("host@1.0.0"));
        assert!(!is_opt("also-required@1.0.0"));
        assert!(!is_opt("shared@1.0.0"));
        // Reachable both optionally and via a required edge → stays required.
        assert!(!is_opt("dual@1.0.0"));
        // Reachable only through optional edges → optional.
        assert!(is_opt("native-darwin@1.0.0"));
        assert!(is_opt("native-linux@1.0.0"));
        // Direct optional importer dep with no required path → optional.
        assert!(is_opt("opt-root@1.0.0"));
    }

    // The install's build-failure classification reads this set directly rather
    // than `LockedPackage::optional`, so the graph — not the stamped field — has
    // to be what decides. Every package here is left `optional: false` (the
    // state a frozen install off an npm / bun / yarn lockfile arrives in): if
    // the classification ever regressed to reading the field, this test would
    // find nothing optional at all.
    #[test]
    fn optional_only_packages_reads_edges_not_the_stamped_flag() {
        use aube_lockfile::DepType;
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![
                dep("host", DepType::Production),
                dep("also-required", DepType::Production),
            ],
        );
        graph.packages.extend([
            pkg("host", &["native", "dual"], &["native", "dual"]),
            pkg("also-required", &["dual"], &[]),
            pkg("native", &["native-child"], &[]),
            pkg("native-child", &[], &[]),
            pkg("dual", &[], &[]),
        ]);
        assert!(
            graph.packages.values().all(|p| !p.optional),
            "fixture must leave the stamped flag unset"
        );

        let optional = optional_only_packages(&graph);

        // Reachable only under `host`'s optional edge — and so is its own child.
        assert!(optional.contains("native@1.0.0"));
        assert!(optional.contains("native-child@1.0.0"));
        // The control that proves this can't swallow a required package's build
        // failure: `dual` hangs off an optional edge from `host` AND a required
        // edge from `also-required`, so one fully-required path keeps it required.
        assert!(!optional.contains("dual@1.0.0"));
        assert!(!optional.contains("host@1.0.0"));
        assert!(!optional.contains("also-required@1.0.0"));
    }

    fn pkg_with_peers(
        name: &str,
        deps: &[&str],
        peers: &[&str],
    ) -> (String, aube_lockfile::LockedPackage) {
        let (key, mut p) = pkg(name, deps, &[]);
        p.peer_dependencies = peers
            .iter()
            .map(|d| ((*d).to_string(), "*".to_string()))
            .collect();
        (key, p)
    }

    #[test]
    fn transitive_peer_dependencies_bubble_unresolved_peers() {
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.extend([
            pkg("app", &["host", "mid"], &[]),
            // `host` declares `core` as a peer AND resolves it (core is in
            // deps, mirrored like pnpm), so nothing bubbles from host.
            pkg_with_peers("host", &["core"], &["core"]),
            pkg("core", &[], &[]),
            // `mid` -> `leaf`, and `leaf` peers on an unresolved
            // `supports-color` (not in its deps): it must bubble to ancestors.
            pkg("mid", &["leaf"], &[]),
            pkg_with_peers("leaf", &["ms"], &["supports-color"]),
            pkg("ms", &[], &[]),
        ]);

        mark_transitive_peer_dependencies(&mut graph);

        let tp = |k: &str| graph.packages[k].transitive_peer_dependencies.clone();
        // Unresolved peer bubbles to every ancestor of `leaf`.
        assert_eq!(tp("app@1.0.0"), vec!["supports-color".to_string()]);
        assert_eq!(tp("mid@1.0.0"), vec!["supports-color".to_string()]);
        // `leaf` declares the peer itself → not in its OWN transitive list.
        assert!(tp("leaf@1.0.0").is_empty());
        assert!(tp("ms@1.0.0").is_empty());
        // `host` resolves its `core` peer → nothing unresolved to bubble.
        assert!(tp("host@1.0.0").is_empty());
        assert!(tp("core@1.0.0").is_empty());
    }

    #[test]
    fn transitive_peer_dependencies_handle_cycles_without_self() {
        let mut graph = aube_lockfile::LockfileGraph::default();
        // a <-> b dependency cycle, each with a distinct unresolved peer.
        graph.packages.extend([
            pkg_with_peers("a", &["b"], &["pa"]),
            pkg_with_peers("b", &["a"], &["pb"]),
        ]);

        mark_transitive_peer_dependencies(&mut graph);

        // Each node collects the other's peer through the cycle but never its
        // own — `a` doesn't list `pa`, `b` doesn't list `pb`.
        assert_eq!(
            graph.packages["a@1.0.0"].transitive_peer_dependencies,
            vec!["pb".to_string()]
        );
        assert_eq!(
            graph.packages["b@1.0.0"].transitive_peer_dependencies,
            vec!["pa".to_string()]
        );
    }

    #[test]
    fn filter_graph_prunes_npm_lockfile_transitive_optional_platform_mismatch() {
        let content = r#"{
            "name": "platform-optional-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "platform-optional-root",
                    "version": "1.0.0",
                    "dependencies": { "host": "file:host" }
                },
                "node_modules/host": {
                    "resolved": "host",
                    "link": true
                },
                "host": {
                    "name": "host",
                    "version": "1.0.0",
                    "optionalDependencies": { "native-win": "1.0.0" }
                },
                "node_modules/native-win": {
                    "version": "1.0.0",
                    "resolved": "https://registry.npmjs.org/native-win/-/native-win-1.0.0.tgz",
                    "integrity": "sha512-native",
                    "optional": true,
                    "os": ["win32"],
                    "cpu": ["x64"],
                    "libc": ["glibc"]
                }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), content).unwrap();
        let mut graph =
            aube_lockfile::npm::parse(tmp.path(), &aube_manifest::PackageJson::default()).unwrap();

        let host_dep_path = graph.importers["."][0].dep_path.clone();
        assert!(
            graph.packages.contains_key(&host_dep_path),
            "fixture must contain the host package before filtering"
        );
        assert!(
            graph.packages.contains_key("native-win@1.0.0"),
            "fixture must contain native-win before filtering"
        );
        let host = &graph.packages[&host_dep_path];
        assert!(host.dependencies.contains_key("native-win"));
        assert!(host.optional_dependencies.contains_key("native-win"));

        let supported = SupportedArchitectures {
            os: s(&["linux"]),
            cpu: s(&["x64"]),
            libc: s(&["glibc"]),
            ..Default::default()
        };
        filter_graph(&mut graph, &supported, &Default::default());

        assert!(graph.packages.contains_key(&host_dep_path));
        assert!(!graph.packages.contains_key("native-win@1.0.0"));
        let host = &graph.packages[&host_dep_path];
        assert!(!host.dependencies.contains_key("native-win"));
        assert!(!host.optional_dependencies.contains_key("native-win"));
    }

    /// `bundleDependencies` are transitive: the parent's tarball ships
    /// the bundled dep AND its whole closure, so on the frozen-restore
    /// path the parent→bundle edge must be cut and the now-unreachable
    /// closure GC'd — otherwise it is promoted to standalone fetch/link
    /// nodes (redundant online, fatal offline).
    #[test]
    fn filter_graph_severs_bundled_edges_and_gcs_closure() {
        use aube_lockfile::DepType;
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .importers
            .insert(".".to_string(), vec![dep("host", DepType::Production)]);
        // `host` bundles `gyp` (which drags in `gyp-dep`) and also has a
        // normal, non-bundled `plain` dep that must survive.
        let (host_key, mut host) = pkg("host", &["gyp", "gyp-dep", "plain"], &[]);
        host.bundled_dependencies = vec!["gyp".to_string(), "gyp-dep".to_string()];
        graph.packages.extend([
            (host_key.clone(), host),
            pkg("gyp", &["gyp-dep"], &[]),
            pkg("gyp-dep", &[], &[]),
            pkg("plain", &[], &[]),
        ]);

        filter_graph(
            &mut graph,
            &SupportedArchitectures::default(),
            &Default::default(),
        );

        let host = &graph.packages[&host_key];
        assert!(
            !host.dependencies.contains_key("gyp"),
            "bundled edge severed"
        );
        assert!(
            !host.dependencies.contains_key("gyp-dep"),
            "bundled edge severed"
        );
        assert!(
            host.dependencies.contains_key("plain"),
            "non-bundled edge kept"
        );
        // The bundled closure is unreachable after the cut → GC'd.
        assert!(!graph.packages.contains_key("gyp@1.0.0"));
        assert!(!graph.packages.contains_key("gyp-dep@1.0.0"));
        assert!(graph.packages.contains_key("plain@1.0.0"));
        assert!(graph.packages.contains_key(&host_key));
    }

    /// Cutting a bundled edge is scoped to the declaring parent: a
    /// package the parent bundles can ALSO be a legitimate standalone
    /// dep of another package, and that consumer's edge (and the shared
    /// node) must survive.
    #[test]
    fn filter_graph_keeps_bundled_name_for_non_bundled_consumer() {
        use aube_lockfile::DepType;
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![
                dep("host", DepType::Production),
                dep("app", DepType::Production),
            ],
        );
        let (host_key, mut host) = pkg("host", &["gyp"], &[]);
        host.bundled_dependencies = vec!["gyp".to_string()];
        graph.packages.extend([
            (host_key.clone(), host),
            pkg("app", &["gyp"], &[]), // legit, non-bundled consumer
            pkg("gyp", &[], &[]),
        ]);

        filter_graph(
            &mut graph,
            &SupportedArchitectures::default(),
            &Default::default(),
        );

        assert!(
            !graph.packages[&host_key].dependencies.contains_key("gyp"),
            "host's bundled edge cut"
        );
        assert!(
            graph.packages["app@1.0.0"].dependencies.contains_key("gyp"),
            "app's legit edge kept"
        );
        assert!(
            graph.packages.contains_key("gyp@1.0.0"),
            "shared node survives via app"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn libc_ignored_off_linux() {
        // On a non-Linux host, a package that declares libc=musl
        // should still pass — npm only enforces libc on Linux.
        let sup = SupportedArchitectures::default();
        assert!(is_supported(&[], &[], &s(&["musl"]), &sup));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_glibc_host_rejects_musl_only_package() {
        // The mirror of `libc_ignored_off_linux`: on a glibc Linux
        // host, a package that declares libc=musl must not pass.
        // Skipped on musl Linux builds, since "current" expands to
        // musl there and the package would (correctly) match.
        if cfg!(target_env = "musl") {
            return;
        }
        let sup = SupportedArchitectures::default();
        assert!(!is_supported(&[], &[], &s(&["musl"]), &sup));
    }

    // --- GVS prewarm/link graph-hash agreement (parcel worker-farm regression) ---

    // A parcel-worker-farm-shaped graph: `@parcel/core` peers with
    // `@parcel/workers`, and pulls in `@parcel/watcher`, whose subtree
    // carries a platform-specific optional native variant per OS — exactly
    // the shape (`@parcel/watcher-<platform>`, `@swc/core-<platform>`, lmdb,
    // `@next/swc-<platform>`) that made the resolver widen the graph for a
    // portable lockfile. Built with `optional_dependencies` set so
    // `filter_graph` drops the host-mismatched leaf.
    fn parcel_worker_farm_graph() -> aube_lockfile::LockfileGraph {
        use aube_lockfile::{DirectDep, LockedPackage, LockfileGraph};
        let mk = |name: &str, dep_path: &str| LockedPackage {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            integrity: Some(format!("sha512-{name}")),
            dep_path: dep_path.to_string(),
            ..Default::default()
        };
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "parcel".to_string(),
                dep_path: "parcel@2.12.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("2.12.0".to_string()),
            }],
        );

        let mut parcel = mk("parcel", "parcel@2.12.0");
        parcel.dependencies = [
            ("@parcel/core".to_string(), "2.12.0".to_string()),
            (
                "@parcel/workers".to_string(),
                "2.12.0(@parcel/core@2.12.0)".to_string(),
            ),
        ]
        .into();
        graph.packages.insert("parcel@2.12.0".to_string(), parcel);

        let mut core = mk("@parcel/core", "@parcel/core@2.12.0");
        core.dependencies = [
            (
                "@parcel/workers".to_string(),
                "2.12.0(@parcel/core@2.12.0)".to_string(),
            ),
            ("@parcel/watcher".to_string(), "2.5.6".to_string()),
        ]
        .into();
        graph
            .packages
            .insert("@parcel/core@2.12.0".to_string(), core);

        let mut workers = mk(
            "@parcel/workers",
            "@parcel/workers@2.12.0(@parcel/core@2.12.0)",
        );
        workers.dependencies = [("@parcel/core".to_string(), "2.12.0".to_string())].into();
        graph.packages.insert(
            "@parcel/workers@2.12.0(@parcel/core@2.12.0)".to_string(),
            workers,
        );

        // The native package with per-OS optional variants — the widening
        // source. `watcher-linux` is host-mismatched on the darwin target
        // the test filters against, so `filter_graph` prunes it and the
        // parent `watcher` (and everything above it) re-hashes.
        let mut watcher = mk("@parcel/watcher", "@parcel/watcher@2.5.6");
        watcher.dependencies = [
            ("@parcel/watcher-darwin".to_string(), "2.5.6".to_string()),
            ("@parcel/watcher-linux".to_string(), "2.5.6".to_string()),
        ]
        .into();
        watcher.optional_dependencies = watcher.dependencies.clone();
        graph
            .packages
            .insert("@parcel/watcher@2.5.6".to_string(), watcher);

        let mut darwin = mk("@parcel/watcher-darwin", "@parcel/watcher-darwin@2.5.6");
        darwin.os = s(&["darwin"]).into();
        darwin.cpu = s(&["arm64"]).into();
        graph
            .packages
            .insert("@parcel/watcher-darwin@2.5.6".to_string(), darwin);

        let mut linux = mk("@parcel/watcher-linux", "@parcel/watcher-linux@2.5.6");
        linux.os = s(&["linux"]).into();
        linux.cpu = s(&["x64"]).into();
        graph
            .packages
            .insert("@parcel/watcher-linux@2.5.6".to_string(), linux);

        graph
    }

    #[test]
    fn gvs_prewarm_and_link_agree_only_after_host_filtering_the_widened_graph() {
        // The GVS prewarm materializes into the shared store while hashing the
        // resolver's WIDENED (all-platform) graph; the link phase hashes the
        // host-FILTERED graph. Any package whose subtree contains a
        // platform-specific optional native dep then recursively hashes
        // differently in the two phases, so the SAME dep_path lands at two
        // byte-identical global store dirs — and symlink-shared consumers split
        // across the copies (for parcel: two `@parcel/core` instances, two
        // serializer registries, `DataCloneError` at worker-farm startup). The
        // fix host-filters the prewarm graph so both phases hash the same graph.
        // This pins BOTH halves: a widened-vs-filtered hash genuinely diverges
        // (why the bug existed), and filtering both sides makes every node hash
        // agree (one store dir per dep_path).
        use aube_lockfile::graph_hash::compute_graph_hashes;

        let widened = parcel_worker_farm_graph();
        let host = SupportedArchitectures {
            os: s(&["darwin"]),
            cpu: s(&["arm64"]),
            ..Default::default()
        };
        let core = "@parcel/core@2.12.0";

        let prewarm_widened = compute_graph_hashes(&widened, &|_| false, None);
        let mut filtered = widened.clone();
        filter_graph(&mut filtered, &host, &Default::default());
        let link = compute_graph_hashes(&filtered, &|_| false, None);

        assert!(
            !filtered
                .packages
                .contains_key("@parcel/watcher-linux@2.5.6"),
            "host filter must drop the linux-only native leaf"
        );
        assert_ne!(
            prewarm_widened.node_hash[core], link.node_hash[core],
            "hashing the widened graph must diverge from the host-filtered graph \
             — that divergence is what split @parcel/core into two store dirs"
        );

        let prewarm_fixed = compute_graph_hashes(&filtered, &|_| false, None);
        assert_eq!(
            prewarm_fixed.node_hash, link.node_hash,
            "with the prewarm host-filtering its graph, prewarm and link must \
             agree on every node hash so no dep_path materializes at two dirs"
        );
    }
}
