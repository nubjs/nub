//! Per-direct-dep packument facts the install summary printer surfaces
//! inline with the `+ name@version` listing — currently deprecation
//! status and the registry `latest` dist-tag when it differs from the
//! resolved version. The data has to be snapshotted before the resolver
//! (which owns the packument cache) is dropped at the end of resolution.

use crate::Resolver;
use aube_lockfile::LockfileGraph;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeGatedUpdate {
    pub name: String,
    pub version: String,
}

/// Subset of packument facts the install summary printer wants to
/// render next to a direct-dependency line. Returned only for direct
/// deps where at least one signal is set — the printer skips the badge
/// column when [`Resolver::direct_dep_info`]'s map has no entry.
///
/// Deprecation is a bare flag (not the message string) by design: the
/// full per-version `deprecated` text already surfaces via the WARN
/// pipeline in [`crate::deprecations`][crate-deprecations] above the
/// summary, so the badge column just signals "this direct dep is one
/// of the WARN lines you saw" without duplicating the message.
///
/// [crate-deprecations]: https://github.com/aubepkg/aube/blob/main/crates/aube/src/deprecations.rs
#[derive(Debug, Clone, Default)]
pub struct DirectDepInfo {
    /// True when the packument marks the *resolved* version as
    /// deprecated. The actual message is intentionally not carried
    /// here — see the struct docs.
    pub deprecated: bool,
    /// The registry's `dist-tags.latest` for this package, but only
    /// when it differs from the resolved version. `None` when latest
    /// matches the resolved version, when the registry omits `latest`
    /// (common on private registries), when `latest` points to a
    /// prerelease (we don't nudge users toward betas), or when the dep
    /// wasn't resolved from a packument (git / file / link / remote
    /// tarball).
    pub latest: Option<String>,
}

impl Resolver {
    /// Return direct updates that an ungated resolve would select but the
    /// active `minimumReleaseAge` policy hides. The selected gated version
    /// must match the resolved graph, which avoids mislabeling a difference
    /// caused by an override, lockfile preference, or another resolver rule.
    pub fn age_gated_updates(&self, graph: &LockfileGraph) -> Vec<AgeGatedUpdate> {
        let Some(minimum_release_age) = self.minimum_release_age.as_ref() else {
            return Vec::new();
        };
        let mut updates = Vec::new();
        for deps in graph.importers.values() {
            for dep in deps {
                let Some(pkg) = graph.packages.get(&dep.dep_path) else {
                    continue;
                };
                if pkg.local_source.is_some() {
                    continue;
                }
                let Some(packument) = self.cache.get(pkg.registry_name()) else {
                    continue;
                };
                let Some(range) = dep.specifier.as_deref() else {
                    continue;
                };
                let crate::PickResult::Found(selected) = crate::pick_version_for_add(
                    packument,
                    pkg.registry_name(),
                    range,
                    Some(minimum_release_age),
                ) else {
                    continue;
                };
                if selected.version != pkg.version {
                    continue;
                }
                let crate::PickResult::Found(ungated) =
                    crate::pick_version_for_add(packument, pkg.registry_name(), range, None)
                else {
                    continue;
                };
                if is_newer(&ungated.version, &selected.version) {
                    updates.push(AgeGatedUpdate {
                        name: dep.name.clone(),
                        version: ungated.version.clone(),
                    });
                }
            }
        }
        updates
    }

    /// Snapshot per-direct-dep packument facts so the install summary
    /// printer can render them inline after the resolver — and its
    /// packument cache — is dropped. Keys are `DirectDep::dep_path`;
    /// importer direct deps don't carry peer-context suffixes, so the
    /// key matches the `LockfileGraph.packages` entry 1:1.
    ///
    /// Skips deps whose packument wasn't fetched (frozen-lockfile reuse,
    /// non-registry sources) and deps whose registry didn't publish a
    /// `latest` dist-tag. Returns only entries where at least one signal
    /// is set so the caller's printer can use `get(dep_path)` as the
    /// "should I render badges?" check.
    pub fn direct_dep_info(&self, graph: &LockfileGraph) -> HashMap<String, DirectDepInfo> {
        let mut out: HashMap<String, DirectDepInfo> = HashMap::new();
        for deps in graph.importers.values() {
            for dep in deps {
                let Some(pkg) = graph.packages.get(&dep.dep_path) else {
                    continue;
                };
                if pkg.local_source.is_some() {
                    continue;
                }
                let Some(packument) = self.cache.get(pkg.registry_name()) else {
                    continue;
                };
                let deprecated = packument
                    .versions
                    .get(&pkg.version)
                    .is_some_and(|v| v.deprecated.is_some());
                let latest = packument
                    .dist_tags
                    .get("latest")
                    .filter(|l| l.as_str() != pkg.version.as_str())
                    .filter(|l| !is_prerelease(l))
                    .cloned();
                if deprecated || latest.is_some() {
                    out.insert(dep.dep_path.clone(), DirectDepInfo { deprecated, latest });
                }
            }
        }
        out
    }
}

/// Whether a version string parses to a semver with a prerelease tag
/// (e.g. `1.2.0-beta.3`, `2.0.0-rc.1`). Unparseable strings are treated
/// as non-prerelease so a registry returning a non-semver `latest`
/// (rare, but possible) still surfaces as an upgrade hint.
fn is_prerelease(version: &str) -> bool {
    node_semver::Version::parse(version)
        .map(|v| !v.pre_release.is_empty())
        .unwrap_or(false)
}

fn is_newer(candidate: &str, selected: &str) -> bool {
    match (
        node_semver::Version::parse(candidate),
        node_semver::Version::parse(selected),
    ) {
        (Ok(candidate), Ok(selected)) => candidate > selected,
        _ => candidate != selected,
    }
}

#[cfg(test)]
mod tests {
    use super::is_prerelease;

    #[test]
    fn detects_prerelease_versions() {
        assert!(is_prerelease("1.0.0-beta.1"));
        assert!(is_prerelease("2.0.0-rc.0"));
        assert!(is_prerelease("0.1.0-alpha"));
    }

    #[test]
    fn stable_versions_are_not_prerelease() {
        assert!(!is_prerelease("1.0.0"));
        assert!(!is_prerelease("0.0.1"));
        assert!(!is_prerelease("not-a-version"));
    }
}
