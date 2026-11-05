//! A by-name index over the existing lockfile graph.
//!
//! The resolver consults the existing lockfile graph three times per
//! task on the warm (add / update / drift) path:
//!
//!   1. `try_lockfile_reuse` — first satisfying, non-vulnerable locked
//!      version for a task's name + range,
//!   2. the inline `locked_version` hint fed to `pick_version`,
//!   3. `existing_local_source_integrity` — the integrity of a locked
//!      package matching a name + version + local source.
//!
//! Each of those is otherwise a `graph.packages.values().find(..)`
//! linear scan over the whole `BTreeMap`, which costs the warm path
//! `O(tasks × lockfile)`. Building a `name → candidates` index once
//! collapses every lookup to the (almost always single-entry) bucket
//! for the task's name.
//!
//! Output identity is the bar: `packages` is a `BTreeMap` keyed by
//! dep_path, so `.values()` yields packages in dep_path order and
//! `.find()` returns the first match in that order. The index is built
//! by iterating `.values()` in that same order and pushing into each
//! name's bucket, so the first element of a bucket is the first
//! `.values()` match — applying the same predicate to the bucket
//! selects the byte-identical package. (`find_satisfying` additionally
//! excludes `inBundle` entries — see its doc — so its predicate is a
//! strict superset of a plain `.values().find()`; the bucket-order
//! identity above still holds among the non-bundled candidates.)

use aube_lockfile::{LocalSource, LockedPackage, LockfileGraph};
use smallvec::SmallVec;
use std::collections::BTreeMap;

use crate::FxHashMap;
use crate::resolve::vulnerable::is_vulnerable;
use crate::semver_util::version_satisfies;

/// Borrowed `name → locked packages` index over an existing lockfile
/// graph. Buckets preserve `BTreeMap::values()` (dep_path) order, so
/// the first matching entry in a bucket is identical to the first
/// match a `graph.packages.values().find(..)` would have returned.
///
/// The single-version case (the overwhelming majority of names in a
/// lockfile) stays inline in the `SmallVec` with no heap allocation;
/// only names with multiple locked versions (`@types/node`,
/// `typescript`, transitively-duplicated utilities) spill to the heap.
pub struct LockedIndex<'a> {
    by_name: FxHashMap<&'a str, SmallVec<[&'a LockedPackage; 1]>>,
}

impl<'a> LockedIndex<'a> {
    /// Build the index from an optional existing graph. Returns an
    /// empty index when there is no existing lockfile (the cold path),
    /// matching the `existing.and_then(..)` short-circuit at the scan
    /// sites.
    pub fn new(existing: Option<&'a LockfileGraph>) -> Self {
        let mut by_name: FxHashMap<&'a str, SmallVec<[&'a LockedPackage; 1]>> =
            FxHashMap::default();
        if let Some(graph) = existing {
            // Iterate in BTreeMap (dep_path) order so each bucket's
            // first entry is the first `.values().find(..)` match.
            for pkg in graph.packages.values() {
                by_name.entry(pkg.name.as_str()).or_default().push(pkg);
            }
        }
        Self { by_name }
    }

    fn bucket(&self, name: &str) -> &[&'a LockedPackage] {
        self.by_name
            .get(name)
            .map(SmallVec::as_slice)
            .unwrap_or(&[])
    }

    /// First locked package whose name matches and whose version both
    /// satisfies `range` and is not vulnerable. The vulnerability check
    /// is part of the predicate, so a vulnerable match is *skipped* and
    /// the search continues. This is `try_lockfile_reuse`'s reuse-target
    /// selector (it calls straight through here).
    ///
    /// `inBundle` entries are excluded: a package that ships inside an
    /// ancestor's tarball is materialized from there, never fetched or
    /// linked as a standalone node, so it is not a valid reuse target.
    /// Without the exclusion a legitimate consumer of the same name
    /// could bind to the integrity-less bundled entry (it sorts ahead of
    /// the real standalone entry in nested dep_path order) and fail
    /// offline. The real standalone entry, if any, is selected instead.
    ///
    /// Packages with a source of their own are excluded too: a range binds
    /// to the registry, here as on a fresh resolve. Otherwise a URL or git
    /// package keeps its source after the manifest entry or override that
    /// asked for it is gone, because its version still satisfies the range.
    pub fn find_satisfying(
        &self,
        name: &str,
        range: &str,
        registry_name: &str,
        vulnerable_ranges: &BTreeMap<String, Vec<String>>,
    ) -> Option<&'a LockedPackage> {
        self.bucket(name).iter().copied().find(|p| {
            !p.in_bundle
                && p.local_source.is_none()
                && version_satisfies(&p.version, range)
                && !is_vulnerable(registry_name, &p.version, vulnerable_ranges)
        })
    }

    /// First locked package whose name matches and whose version
    /// satisfies `range`, with NO vulnerability check. Mirrors the
    /// `locked_version` hint scan, which does
    /// `.find(|p| name && satisfies)` then drops the *single* result if
    /// it turns out vulnerable (the caller applies `.filter(..)`) —
    /// so a vulnerable first match is *not* replaced by a later
    /// non-vulnerable one. Returning the candidate (rather than folding
    /// the filter in here) preserves that exact difference.
    pub fn find_first_in_range(&self, name: &str, range: &str) -> Option<&'a LockedPackage> {
        self.bucket(name)
            .iter()
            .copied()
            .find(|p| version_satisfies(&p.version, range))
    }

    /// The specifier behind a hashed local edge (`file+<hash>`, `link+<hash>`),
    /// which is what a reused parent's edge has to be to re-resolve. `None` for
    /// any edge that does not land on a local package.
    pub fn local_specifier(&self, name: &str, tail: &str) -> Option<String> {
        self.local_source(name, tail).map(LocalSource::specifier)
    }

    /// The local source a hashed edge lands on, if it lands on one.
    pub fn local_source(&self, name: &str, tail: &str) -> Option<&'a LocalSource> {
        self.bucket(name)
            .iter()
            .copied()
            .find(|pkg| {
                pkg.dep_path
                    .strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('@'))
                    == Some(tail)
            })
            .and_then(|pkg| pkg.local_source.as_ref())
    }

    /// Integrity of the first locked package matching `name` + a local
    /// source that matches `local` (with the git placeholder-version
    /// allowance). Mirrors `existing_local_source_integrity`.
    pub fn find_local_source_integrity(
        &self,
        name: &str,
        version: &str,
        local: &LocalSource,
    ) -> Option<String> {
        self.bucket(name)
            .iter()
            .copied()
            .find(|pkg| {
                pkg.local_source.as_ref().is_some_and(|old| {
                    local_sources_match_for_integrity(old, local)
                        && (pkg.version == version
                            || matches!((old, local), (LocalSource::Git(_), LocalSource::Git(_)))
                                && pkg.version == "0.0.0")
                })
            })
            .and_then(|pkg| pkg.integrity.clone())
    }
}

fn local_sources_match_for_integrity(old: &LocalSource, new: &LocalSource) -> bool {
    match (old, new) {
        (LocalSource::Git(old), LocalSource::Git(new)) => {
            aube_lockfile::git_commits_match(&old.resolved, &new.resolved)
                && old.subpath == new.subpath
        }
        _ => old == new,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::GitSource;

    fn locked(name: &str, version: &str) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    /// The index must select the byte-identical package a
    /// `graph.packages.values().find(..)` would: the FIRST match in
    /// dep_path (BTreeMap) order. Two locked versions of one name —
    /// confirm the index returns the same one the linear scan does
    /// for both the `find_satisfying` and `find_first_in_range` paths.
    #[test]
    fn index_selects_same_package_as_linear_scan() {
        // dep_paths sort lexicographically; `react@17` sorts before
        // `react@18`, so a `>=16` range's first `.values()` match is
        // 17, not 18.
        let graph = LockfileGraph {
            packages: BTreeMap::from([
                ("react@17.0.2".to_string(), locked("react", "17.0.2")),
                ("react@18.2.0".to_string(), locked("react", "18.2.0")),
                ("lodash@4.17.21".to_string(), locked("lodash", "4.17.21")),
            ]),
            ..Default::default()
        };
        let vuln = BTreeMap::new();
        let index = LockedIndex::new(Some(&graph));

        // Reference: the exact linear scan the index replaces.
        let linear = |name: &str, range: &str| {
            graph
                .packages
                .values()
                .find(|p| p.name == name && version_satisfies(&p.version, range))
        };

        for (name, range) in [("react", ">=16"), ("react", "^18"), ("lodash", "^4")] {
            let want = linear(name, range);
            assert_eq!(
                index.find_first_in_range(name, range).map(|p| &p.version),
                want.map(|p| &p.version),
                "find_first_in_range diverged from linear scan for {name}@{range}",
            );
            // With no vulnerable ranges, find_satisfying matches too.
            assert_eq!(
                index
                    .find_satisfying(name, range, name, &vuln)
                    .map(|p| &p.version),
                want.map(|p| &p.version),
                "find_satisfying diverged from linear scan for {name}@{range}",
            );
        }
    }

    /// `find_satisfying` skips a vulnerable first match and continues;
    /// `find_first_in_range` does not (the caller post-filters and so
    /// drops it). This difference is load-bearing — the two scan sites
    /// behave differently here.
    #[test]
    fn vulnerable_first_match_skipped_only_by_find_satisfying() {
        let graph = LockfileGraph {
            packages: BTreeMap::from([
                ("pkg@1.0.0".to_string(), locked("pkg", "1.0.0")),
                ("pkg@1.5.0".to_string(), locked("pkg", "1.5.0")),
            ]),
            ..Default::default()
        };
        // 1.0.0 (the first match) is flagged vulnerable.
        let mut vuln = BTreeMap::new();
        vuln.insert("pkg".to_string(), vec!["1.0.0".to_string()]);
        let index = LockedIndex::new(Some(&graph));

        // find_satisfying skips 1.0.0, returns 1.5.0.
        assert_eq!(
            index
                .find_satisfying("pkg", "^1", "pkg", &vuln)
                .map(|p| p.version.as_str()),
            Some("1.5.0"),
        );
        // find_first_in_range still returns 1.0.0 (caller then drops it).
        assert_eq!(
            index
                .find_first_in_range("pkg", "^1")
                .map(|p| p.version.as_str()),
            Some("1.0.0"),
        );
    }

    #[test]
    fn empty_index_when_no_existing_graph() {
        let index = LockedIndex::new(None);
        assert!(index.find_first_in_range("anything", "*").is_none());
        let vuln = BTreeMap::new();
        assert!(
            index
                .find_satisfying("anything", "*", "anything", &vuln)
                .is_none()
        );
    }

    #[test]
    fn find_local_source_integrity_matches_resolved_git_commit() {
        let source = LocalSource::Git(GitSource {
            url: "git+https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: None,
        });
        let graph = LockfileGraph {
            packages: BTreeMap::from([(
                "dep@git+https://github.com/acme/dep.git#abcdef0123456789abcdef0123456789abcdef01"
                    .to_string(),
                LockedPackage {
                    name: "dep".to_string(),
                    version: "1.0.0".to_string(),
                    integrity: Some("sha512-old".to_string()),
                    local_source: Some(source.clone()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let index = LockedIndex::new(Some(&graph));

        assert_eq!(
            index
                .find_local_source_integrity("dep", "1.0.0", &source)
                .as_deref(),
            Some("sha512-old")
        );

        let changed_commit = LocalSource::Git(GitSource {
            resolved: "1111111111111111111111111111111111111111".to_string(),
            ..match source {
                LocalSource::Git(g) => g,
                _ => unreachable!(),
            }
        });
        assert!(
            index
                .find_local_source_integrity("dep", "1.0.0", &changed_commit)
                .is_none()
        );
    }

    #[test]
    fn find_local_source_integrity_matches_git_by_resolved_commit() {
        let old_source = LocalSource::Git(GitSource {
            url: "git+ssh://git@github.com/acme/dep.git".to_string(),
            committish: None,
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: Some("packages/dep".to_string()),
        });
        let graph = LockfileGraph {
            packages: BTreeMap::from([(
                "dep@git+ssh://git@github.com/acme/dep.git#abcdef0123456789abcdef0123456789abcdef01"
                    .to_string(),
                LockedPackage {
                    name: "dep".to_string(),
                    version: "1.0.0".to_string(),
                    integrity: Some("sha512-old".to_string()),
                    local_source: Some(old_source),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let resolved_source = LocalSource::Git(GitSource {
            url: "https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: Some("packages/dep".to_string()),
        });
        let index = LockedIndex::new(Some(&graph));

        assert_eq!(
            index
                .find_local_source_integrity("dep", "1.0.0", &resolved_source)
                .as_deref(),
            Some("sha512-old")
        );
    }

    #[test]
    fn find_local_source_integrity_matches_git_abbrev_and_placeholder_version() {
        let old_source = LocalSource::Git(GitSource {
            url: "git+ssh://git@github.com/acme/dep.git".to_string(),
            committish: None,
            resolved: "abcdef0".to_string(),
            integrity: None,
            subpath: None,
        });
        let graph = LockfileGraph {
            packages: BTreeMap::from([(
                "dep@git+ssh://git@github.com/acme/dep.git#abcdef0".to_string(),
                LockedPackage {
                    name: "dep".to_string(),
                    version: "0.0.0".to_string(),
                    integrity: Some("sha512-old".to_string()),
                    local_source: Some(old_source),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let resolved_source = LocalSource::Git(GitSource {
            url: "https://github.com/acme/dep.git".to_string(),
            committish: Some("main".to_string()),
            resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            integrity: None,
            subpath: None,
        });
        let index = LockedIndex::new(Some(&graph));

        assert_eq!(
            index
                .find_local_source_integrity("dep", "1.0.0", &resolved_source)
                .as_deref(),
            Some("sha512-old")
        );
    }
}
