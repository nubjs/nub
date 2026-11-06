use crate::semver_util::{normalize_range, strip_alias_prefix, version_satisfies};
use crate::{FxHashMap, PackageExtension, override_rule};
use aube_lockfile::LockfileGraph;
use std::collections::BTreeMap;

/// Find the best-matching override rule for a task and return its
/// replacement spec (cloned). "Best" means most specific: we score
/// each matching rule by `non_wildcard_parents * 2 +
/// (target_version_req ? 1 : 0)` and take the max, so `a>b>c` beats
/// `b>c` beats `c`, and a version-qualified `c@<2` beats a bare `c`.
/// Wildcard `**` parent segments don't inflate the score — `**/foo`
/// is semantically equivalent to a bare `foo` and shouldn't
/// out-rank a more specific `foo@<2`. Ties break on rule insertion
/// order (stable `iter()` over a `Vec`), which reflects the
/// manifest's BTreeMap ordering after pnpm/yarn precedence merging.
pub(crate) fn pick_override_spec(
    rules: &[override_rule::OverrideRule],
    task_name: &str,
    task_range: &str,
    ancestors: &[(String, String)],
) -> Option<String> {
    // When the task range is an `npm:`/`jsr:` alias, the trailing
    // `@<version>` — not the raw alias string — is what should
    // participate in a selector's version-range check. Without this
    // normalization, the matcher's `range_could_satisfy` never
    // parses the raw `npm:@scope/pkg@6.0.9-patched.1` as a semver,
    // hits its "probably matches" fallback, and fires overrides
    // whose version req (`>=7 <9`) the real version doesn't satisfy.
    // Reported in #174.
    let effective_range = strip_alias_prefix(task_range);
    pick_best(rules, ancestors, |r, frames| {
        override_rule::matches(r, task_name, effective_range, frames)
    })
}

/// [`pick_override_spec`] for a dependency declared as `catalog_spec`
/// (`catalog:` / `catalog:<name>`), matched the way pnpm matches it: against
/// the reference text, before the catalog is consulted. See
/// [`override_rule::matches_catalog_reference`].
pub(crate) fn pick_catalog_override_spec(
    rules: &[override_rule::OverrideRule],
    task_name: &str,
    catalog_spec: &str,
    ancestors: &[(String, String)],
) -> Option<String> {
    pick_best(rules, ancestors, |r, frames| {
        override_rule::matches_catalog_reference(r, task_name, catalog_spec, frames)
    })
}

fn pick_best(
    rules: &[override_rule::OverrideRule],
    ancestors: &[(String, String)],
    is_match: impl Fn(&override_rule::OverrideRule, &[override_rule::AncestorFrame<'_>]) -> bool,
) -> Option<String> {
    let frames: Vec<override_rule::AncestorFrame<'_>> = ancestors
        .iter()
        .map(|(n, v)| override_rule::AncestorFrame {
            name: n,
            version: v,
        })
        .collect();
    rules
        .iter()
        .filter(|r| is_match(r, &frames))
        .max_by_key(|r| {
            let named_parents = r.parents.iter().filter(|p| !p.is_wildcard()).count();
            named_parents * 2 + usize::from(r.target.version_req.is_some())
        })
        .map(|r| r.replacement.clone())
}

pub(crate) fn apply_package_extensions(
    pkg: &mut aube_registry::VersionMetadata,
    extensions: &[PackageExtension],
) {
    for extension in extensions {
        if !package_selector_matches(&extension.selector, &pkg.name, &pkg.version) {
            continue;
        }
        extend_missing(&mut pkg.dependencies, &extension.dependencies);
        extend_missing(
            &mut pkg.optional_dependencies,
            &extension.optional_dependencies,
        );
        extend_missing(&mut pkg.peer_dependencies, &extension.peer_dependencies);
        extend_missing(
            &mut pkg.peer_dependencies_meta,
            &extension.peer_dependencies_meta,
        );
    }
}

/// Apply `packageExtensions` to a non-registry (git / remote-tarball /
/// directory) package's flat `name -> range` dependency map.
///
/// The registry resolve path applies extensions to the picked
/// [`aube_registry::VersionMetadata`] via [`apply_package_extensions`],
/// but git/tarball/directory packages resolve through a separate path
/// whose only dependency surface is a flat map (the package's own
/// `dependencies`). Without this, a `packageExtensions` entry targeting
/// a git dependency — e.g. injecting a connector a package `require()`s
/// dynamically at runtime so it lands as a sibling in the global virtual
/// store — is silently dropped, never resolved, and never linked.
///
/// Only `dependencies` are merged: the non-registry resolve path models
/// a package's transitive surface as `dependencies` alone (it does not
/// enqueue a git package's own `optionalDependencies`/`peerDependencies`),
/// so injecting the other maps here would resolve them under stricter
/// semantics than the package's own deps. `extend_missing` keeps the
/// package's declared deps authoritative.
pub(crate) fn apply_package_extensions_to_deps(
    name: &str,
    version: &str,
    deps: &mut BTreeMap<String, String>,
    extensions: &[PackageExtension],
) {
    for extension in extensions {
        if !package_selector_matches(&extension.selector, name, version) {
            continue;
        }
        extend_missing(deps, &extension.dependencies);
    }
}

/// The `database_peers` each package in `graph` takes, by key: `peer -> range`.
///
/// A peer the package's manifest or a curated catalog already declares stays a
/// real peer, and non-registry packages take no extensions but `dependencies`,
/// as on the resolve path. The first matching entry wins per peer, as in
/// [`extend_missing`].
pub(crate) fn database_peers_by_package(
    graph: &LockfileGraph,
    extensions: &[PackageExtension],
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut by_name: FxHashMap<&str, Vec<&PackageExtension>> = FxHashMap::default();
    for extension in extensions.iter().filter(|e| !e.database_peers.is_empty()) {
        let selector = extension.selector.trim();
        let name = split_package_selector(selector).map_or(selector, |(name, _)| name);
        by_name.entry(name).or_default().push(extension);
    }
    let mut result = BTreeMap::new();
    if by_name.is_empty() {
        return result;
    }
    for (key, pkg) in &graph.packages {
        if pkg.local_source.is_some() {
            continue;
        }
        let Some(candidates) = by_name.get(pkg.registry_name()) else {
            continue;
        };
        let mut peers: BTreeMap<String, String> = BTreeMap::new();
        for extension in candidates {
            if !package_selector_matches(&extension.selector, pkg.registry_name(), &pkg.version) {
                continue;
            }
            for (peer, range) in &extension.database_peers {
                if *peer != pkg.name
                    && !pkg.dependencies.contains_key(peer)
                    && !pkg.peer_dependencies.contains_key(peer)
                    && !pkg.bundled_dependencies.contains(peer)
                {
                    peers.entry(peer.clone()).or_insert_with(|| range.clone());
                }
            }
        }
        if !peers.is_empty() {
            result.insert(key.clone(), peers);
        }
    }
    result
}

/// Whether a database entry names `dep_name` for this package, so a lockfile's
/// optional edge to it may be that repair rather than a declared dependency.
pub(crate) fn is_database_peer(
    extensions: &[PackageExtension],
    name: &str,
    version: &str,
    dep_name: &str,
) -> bool {
    extensions.iter().any(|extension| {
        extension.database_peers.contains_key(dep_name)
            && package_selector_matches(&extension.selector, name, version)
    })
}

fn extend_missing<K, V>(target: &mut BTreeMap<K, V>, additions: &BTreeMap<K, V>)
where
    K: Ord + Clone,
    V: Clone,
{
    for (key, value) in additions {
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

pub(crate) fn package_selector_matches(selector: &str, name: &str, version: &str) -> bool {
    let selector = selector.trim();
    if selector == name {
        return true;
    }
    let Some((selector_name, range)) = split_package_selector(selector) else {
        return false;
    };
    if selector_name != name {
        return false;
    }
    // A `*` (or empty) range means "any version" and must hold even when
    // `version` isn't parseable semver — git/tarball packages can carry
    // odd version strings. `version_satisfies` returns false for an
    // unparseable version, so short-circuit `*` to keep `name@*`
    // extensions matching non-registry packages. Registry packages
    // always have valid semver, so this never changes their outcome.
    let range = normalize_range(range);
    if range == "*" {
        return true;
    }
    version_satisfies(version, range)
}

fn split_package_selector(selector: &str) -> Option<(&str, &str)> {
    let at = selector.rfind('@')?;
    if at == 0 {
        return None;
    }
    if selector.starts_with('@') {
        let slash = selector.find('/')?;
        if at <= slash {
            return None;
        }
    }
    let (name, range) = selector.split_at(at);
    let range = &range[1..];
    (!name.is_empty() && !range.is_empty()).then_some((name, range))
}

/// Honor `allowedDeprecatedVersions`: does the pinned range (keyed by
/// package name) mute the deprecation warning for this specific version?
/// Used by the resolver's fresh-resolve path and by `aube deprecations`.
pub fn is_deprecation_allowed(
    name: &str,
    version: &str,
    allowed: &BTreeMap<String, String>,
) -> bool {
    allowed
        .get(name)
        .is_some_and(|range| version_satisfies(version, range))
}
