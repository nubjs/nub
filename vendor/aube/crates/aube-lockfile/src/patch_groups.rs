//! Patch-key grouping + per-package resolution — a port of pnpm's
//! `groupPatchedDependencies` (`patching/config`) and `getPatchInfo`.
//!
//! A `patchedDependencies` key is one of four shapes: an exact
//! `name@1.2.3`, a semver range `name@>=1`, a wildcard `name@*`, or a
//! bare `name`. The last two mean "patch every resolved version of the
//! package". [`PatchGroups`] indexes the declared keys by package name;
//! [`PatchGroups::resolve`] maps a concrete resolved `name@version` to
//! the single patch that applies, by pnpm's priority: an exact match
//! wins over a range match wins over an "all" (wildcard/bare) match.
//! Two ranges matching one version is a hard error, exactly as pnpm's
//! `PATCH_KEY_CONFLICT`.
//!
//! Callers keep the *source key* (the verbatim declared string) as the
//! lockfile identity — pnpm records it unresolved in the
//! `patchedDependencies` block — and use the resolved concrete
//! `name@version` only for the linker apply, the graph-hash fold, and
//! the `(patch_hash=…)` dep-path suffix.

use std::collections::BTreeMap;

/// Classification of a single `patchedDependencies` key. The `name` is
/// the group a resolved version is matched against; borrows the source
/// key so grouping keeps the verbatim string as the patch identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchKeyForm<'a> {
    /// `name@1.2.3` — patches exactly that version.
    Exact { name: &'a str, version: &'a str },
    /// `name@<range>` — patches the resolved version if it satisfies.
    Range { name: &'a str, range: &'a str },
    /// `name@*` or bare `name` — patches every resolved version.
    All { name: &'a str },
}

/// A `patchedDependencies` key carried a non-`*` version selector that
/// is not a valid semver range (pnpm's `PATCH_NON_SEMVER_RANGE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPatchRange {
    pub range: String,
}

impl InvalidPatchRange {
    /// Byte-identical to pnpm's `PATCH_NON_SEMVER_RANGE` message.
    pub fn message(&self) -> String {
        format!("{} is not a valid semantic version range.", self.range)
    }
}

/// Two or more range keys matched one resolved version, so the patch is
/// ambiguous (pnpm's `PATCH_KEY_CONFLICT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchKeyConflict {
    /// `name@version` of the package that matched multiple ranges.
    pub pkg_id: String,
    /// The satisfying range selectors, in group order.
    pub ranges: Vec<String>,
}

impl PatchKeyConflict {
    /// pnpm's `PATCH_KEY_CONFLICT` message. The satisfying-range SET is
    /// identical to pnpm's; the list ORDER follows sorted key order
    /// (pnpm lists them in config-declaration order, which is lost when
    /// the keys pass through a sorted `BTreeMap` upstream) — cosmetic,
    /// in an error path.
    pub fn message(&self) -> String {
        format!(
            "Unable to choose between {} version ranges to patch {}: {}",
            self.ranges.len(),
            self.pkg_id,
            self.ranges.join(", ")
        )
    }

    /// pnpm attaches this as the error hint.
    pub fn hint(&self) -> String {
        format!(
            "Explicitly set the exact version ({}) to resolve conflict",
            self.pkg_id
        )
    }
}

/// Split a patch key into `(name, version_selector)` at the first `@`
/// past index 0, mirroring pnpm's `dp.parse` (`indexOf('@', 1)`). A
/// scope-only `@` at index 0 is not a separator, so a bare scoped name
/// (`@babel/core`) has no version. Returns `None` when there is no
/// separator or the version selector is empty (both route to "all" in
/// pnpm, keyed on the whole string).
fn split_name_selector(key: &str) -> Option<(&str, &str)> {
    // `find` on `key[1..]` skips a scope `@`; offset by 1 to index `key`.
    let at = key.get(1..)?.find('@').map(|i| i + 1)?;
    let selector = &key[at + 1..];
    if selector.is_empty() {
        return None;
    }
    Some((&key[..at], selector))
}

/// Classify one `patchedDependencies` key. Mirrors pnpm's `dp.parse`
/// plus the branch selection in `groupPatchedDependencies`: an exact
/// semver version → `Exact`; otherwise a valid range → `All` when it
/// trims to `*`, else `Range`; an invalid non-`*` selector errors; a
/// bare/empty-selector key → `All`.
pub fn classify_patch_key(key: &str) -> Result<PatchKeyForm<'_>, InvalidPatchRange> {
    let Some((name, selector)) = split_name_selector(key) else {
        return Ok(PatchKeyForm::All { name: key });
    };
    // pnpm's `semver.valid(version)` gate: a parseable exact version is
    // `Exact`, everything else falls through to the range branch.
    if node_semver::Version::parse(selector).is_ok() {
        return Ok(PatchKeyForm::Exact {
            name,
            version: selector,
        });
    }
    if node_semver::Range::parse(selector).is_err() {
        return Err(InvalidPatchRange {
            range: selector.to_string(),
        });
    }
    if selector.trim() == "*" {
        Ok(PatchKeyForm::All { name })
    } else {
        Ok(PatchKeyForm::Range {
            name,
            range: selector,
        })
    }
}

/// One package name's patch selectors. Source keys are borrowed so a
/// caller can look the resolved source key back up in its own path/hash
/// maps.
struct Group<'a> {
    /// `version string → source key`. String-keyed like pnpm's
    /// `exact[pkgVersion]` — an exact match is literal, not semver.
    exact: BTreeMap<&'a str, &'a str>,
    /// `(display range, parsed range, source key)`, in insertion order.
    /// The range is parsed once here so `resolve` doesn't reparse per
    /// package.
    range: Vec<(&'a str, node_semver::Range, &'a str)>,
    /// The wildcard / bare-name catch-all, if declared.
    all: Option<&'a str>,
}

impl Group<'_> {
    fn new() -> Self {
        Group {
            exact: BTreeMap::new(),
            range: Vec::new(),
            all: None,
        }
    }
}

/// Declared `patchedDependencies` keys indexed by package name.
pub struct PatchGroups<'a> {
    groups: BTreeMap<&'a str, Group<'a>>,
}

impl<'a> PatchGroups<'a> {
    /// Build groups from source keys. Callers pass DEDUPLICATED keys —
    /// a duplicate range key would push twice and manufacture a false
    /// conflict; the only union call site (the lockfile writer) dedups
    /// via a set first. Errors on the first invalid range.
    pub fn build(keys: impl Iterator<Item = &'a str>) -> Result<Self, InvalidPatchRange> {
        let mut groups: BTreeMap<&'a str, Group<'a>> = BTreeMap::new();
        for key in keys {
            match classify_patch_key(key)? {
                PatchKeyForm::Exact { name, version } => {
                    groups
                        .entry(name)
                        .or_insert_with(Group::new)
                        .exact
                        .insert(version, key);
                }
                PatchKeyForm::Range { name, range } => {
                    // `classify_patch_key` already validated the range,
                    // so the reparse cannot fail — but avoid `unwrap` at
                    // the boundary and treat a parse miss as "no range".
                    if let Ok(parsed) = node_semver::Range::parse(range) {
                        groups
                            .entry(name)
                            .or_insert_with(Group::new)
                            .range
                            .push((range, parsed, key));
                    }
                }
                PatchKeyForm::All { name } => {
                    groups.entry(name).or_insert_with(Group::new).all = Some(key);
                }
            }
        }
        Ok(PatchGroups { groups })
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Resolve a concrete `name@version` to the source key of the patch
    /// that applies, by pnpm's `getPatchInfo` priority: exact match,
    /// then the single satisfying range (2+ → [`PatchKeyConflict`]),
    /// then the wildcard/bare catch-all. `Ok(None)` when nothing
    /// matches.
    pub fn resolve(&self, name: &str, version: &str) -> Result<Option<&'a str>, PatchKeyConflict> {
        let Some(group) = self.groups.get(name) else {
            return Ok(None);
        };
        if let Some(&source_key) = group.exact.get(version) {
            return Ok(Some(source_key));
        }
        // A non-semver resolved version (git/exotic) parses to `None`;
        // no range can satisfy it, matching `semver.satisfies` returning
        // false, so it falls through to `all`.
        if let Ok(parsed) = node_semver::Version::parse(version) {
            let satisfied: Vec<&(&'a str, node_semver::Range, &'a str)> = group
                .range
                .iter()
                .filter(|(_, r, _)| r.satisfies(&parsed))
                .collect();
            if satisfied.len() > 1 {
                return Err(PatchKeyConflict {
                    pkg_id: format!("{name}@{version}"),
                    ranges: satisfied
                        .iter()
                        .map(|(disp, _, _)| disp.to_string())
                        .collect(),
                });
            }
            if let Some((_, _, source_key)) = satisfied.first() {
                return Ok(Some(source_key));
            }
        }
        Ok(group.all)
    }

    /// Resolve a graph package by the name pnpm would match it under:
    /// its REGISTRY name, never the folder it is installed as.
    ///
    /// pnpm keeps no separate node for an npm-aliased install —
    /// `odd-alias: npm:is-odd@3.0.1` resolves to the `is-odd@3.0.1`
    /// node, and `getPatchInfo(patchedDependencies, pkg.name,
    /// pkg.version)` matches it on the resolved manifest name. So a key
    /// declared against the registry name patches the alias too, and a
    /// key naming the ALIAS matches nothing at all — no manifest is
    /// named `odd-alias`. Aube keeps the alias as its own
    /// `LockedPackage` because the linker needs it to place
    /// `node_modules/<alias>`, so pnpm's rule has to be applied
    /// explicitly here instead of falling out of the graph shape.
    ///
    /// A key only an alias name would match is dead config under this
    /// rule; [`unused_patch_keys`] finds it — along with every other key
    /// nothing matches — so a caller can refuse the install rather than
    /// leave the user with a silently unapplied patch.
    pub fn resolve_package(
        &self,
        pkg: &crate::LockedPackage,
    ) -> Result<Option<&'a str>, PatchKeyConflict> {
        self.resolve(pkg.registry_name(), &pkg.version)
    }
}

/// A declared `patchedDependencies` key that no installed package
/// matches, so the patch it names would never be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedPatchKey<'a> {
    /// The declared selector, verbatim.
    pub source_key: &'a str,
    /// The registry-name spelling that WOULD have matched, when the key
    /// names an npm alias instead of the package behind it. `None` when
    /// the key simply matches nothing installed.
    pub registry_name_spelling: Option<String>,
}

/// Every declared patch key that matches no installed package, in
/// declaration order — pnpm's `verifyPatches`, which fails the install
/// unless `allowUnusedPatches` downgrades it.
///
/// The alias sub-case gets a concrete replacement spelling: swapping
/// only the name prefix preserves whatever version selector the user
/// wrote (exact, range, `*`, or absent), so the suggestion is a drop-in.
/// A per-package conflict is swallowed here; the install's own conflict
/// gate raises it.
pub fn unused_patch_keys<'a>(
    source_keys: impl Iterator<Item = &'a str>,
    packages: &BTreeMap<String, crate::LockedPackage>,
) -> Result<Vec<UnusedPatchKey<'a>>, InvalidPatchRange> {
    let declared: Vec<&'a str> = source_keys.collect();
    let groups = PatchGroups::build(declared.iter().copied())?;
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let matched: std::collections::BTreeSet<&str> = packages
        .values()
        .filter_map(|pkg| groups.resolve_package(pkg).ok().flatten())
        .collect();
    let alias_spelling = |source_key: &str| -> Option<String> {
        packages.values().find_map(|pkg| {
            let registry_name = pkg.alias_of.as_deref()?;
            let hit = groups.resolve(&pkg.name, &pkg.version).ok().flatten()?;
            (hit == source_key).then(|| format!("{registry_name}{}", &source_key[pkg.name.len()..]))
        })
    };
    Ok(declared
        .into_iter()
        .filter(|key| !matched.contains(key))
        .map(|source_key| UnusedPatchKey {
            source_key,
            registry_name_spelling: alias_spelling(source_key),
        })
        .collect())
}

/// Either failure mode of resolving a whole graph's patches — an
/// invalid range in the declared set, or a per-package conflict.
#[derive(Debug, Clone)]
pub enum PatchResolveError {
    InvalidRange(InvalidPatchRange),
    Conflict(PatchKeyConflict),
}

impl From<InvalidPatchRange> for PatchResolveError {
    fn from(e: InvalidPatchRange) -> Self {
        PatchResolveError::InvalidRange(e)
    }
}

/// Resolve a source-key-keyed patch map into one keyed by the concrete
/// `name@version` each package resolves to, applying pnpm's per-package
/// `getPatchInfo`. The value is cloned from the winning source key.
/// Packages are deduplicated by `spec_key` (peer-context variants share
/// one). An all-exact source resolves each key to its own
/// `name@version`, so the output equals the input restricted to
/// installed packages — the pre-resolution behavior for exact keys.
pub fn resolve_patched_by_version<V: Clone>(
    source: &BTreeMap<String, V>,
    packages: &BTreeMap<String, crate::LockedPackage>,
) -> Result<BTreeMap<String, V>, PatchResolveError> {
    if source.is_empty() {
        return Ok(BTreeMap::new());
    }
    let groups = PatchGroups::build(source.keys().map(String::as_str))?;
    let mut out = BTreeMap::new();
    for pkg in packages.values() {
        let spec = pkg.spec_key();
        if out.contains_key(&spec) {
            continue;
        }
        if let Some(source_key) = groups
            .resolve_package(pkg)
            .map_err(PatchResolveError::Conflict)?
            && let Some(value) = source.get(source_key)
        {
            out.insert(spec, value.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_exact_plain_and_scoped() {
        assert_eq!(
            classify_patch_key("is-positive@3.1.0").unwrap(),
            PatchKeyForm::Exact {
                name: "is-positive",
                version: "3.1.0"
            }
        );
        assert_eq!(
            classify_patch_key("@babel/core@7.0.0").unwrap(),
            PatchKeyForm::Exact {
                name: "@babel/core",
                version: "7.0.0"
            }
        );
    }

    #[test]
    fn classify_bare_name_is_all() {
        assert_eq!(
            classify_patch_key("sonda").unwrap(),
            PatchKeyForm::All { name: "sonda" }
        );
        assert_eq!(
            classify_patch_key("@babel/core").unwrap(),
            PatchKeyForm::All {
                name: "@babel/core"
            }
        );
    }

    #[test]
    fn classify_wildcard_is_all() {
        assert_eq!(
            classify_patch_key("is-positive@*").unwrap(),
            PatchKeyForm::All {
                name: "is-positive"
            }
        );
    }

    #[test]
    fn classify_range() {
        assert_eq!(
            classify_patch_key("is-positive@>=3.0.0").unwrap(),
            PatchKeyForm::Range {
                name: "is-positive",
                range: ">=3.0.0"
            }
        );
        assert_eq!(
            classify_patch_key("is-positive@^3").unwrap(),
            PatchKeyForm::Range {
                name: "is-positive",
                range: "^3"
            }
        );
    }

    #[test]
    fn classify_invalid_range_errors_with_pnpm_message() {
        let err = classify_patch_key("is-positive@not-a-range").unwrap_err();
        assert_eq!(
            err.message(),
            "not-a-range is not a valid semantic version range."
        );
    }

    #[test]
    fn resolve_exact_wins_over_range_and_all() {
        let keys = ["foo@1.2.3", "foo@>=1.0.0", "foo"];
        let groups = PatchGroups::build(keys.iter().copied()).unwrap();
        assert_eq!(groups.resolve("foo", "1.2.3").unwrap(), Some("foo@1.2.3"));
    }

    #[test]
    fn resolve_range_matches_satisfying_version() {
        let groups = PatchGroups::build(["foo@>=3.0.0"].into_iter()).unwrap();
        assert_eq!(groups.resolve("foo", "3.1.0").unwrap(), Some("foo@>=3.0.0"));
        assert_eq!(groups.resolve("foo", "2.0.0").unwrap(), None);
    }

    #[test]
    fn resolve_all_patches_every_version() {
        let groups = PatchGroups::build(["sonda"].into_iter()).unwrap();
        assert_eq!(groups.resolve("sonda", "0.9.0").unwrap(), Some("sonda"));
        assert_eq!(groups.resolve("sonda", "1.4.2").unwrap(), Some("sonda"));
    }

    #[test]
    fn resolve_wildcard_behaves_like_bare_name() {
        let groups = PatchGroups::build(["sonda@*"].into_iter()).unwrap();
        assert_eq!(groups.resolve("sonda", "0.9.0").unwrap(), Some("sonda@*"));
    }

    #[test]
    fn resolve_conflicting_ranges_errors_with_pnpm_message() {
        let keys = ["foo@>=3.0.0", "foo@^3.1.0"];
        let groups = PatchGroups::build(keys.iter().copied()).unwrap();
        let err = groups.resolve("foo", "3.1.0").unwrap_err();
        assert_eq!(
            err.message(),
            "Unable to choose between 2 version ranges to patch foo@3.1.0: >=3.0.0, ^3.1.0"
        );
        assert_eq!(
            err.hint(),
            "Explicitly set the exact version (foo@3.1.0) to resolve conflict"
        );
    }

    #[test]
    fn resolve_unknown_name_is_none() {
        let groups = PatchGroups::build(["foo@1.0.0"].into_iter()).unwrap();
        assert_eq!(groups.resolve("bar", "1.0.0").unwrap(), None);
    }

    #[test]
    fn resolve_non_semver_version_falls_through_to_all() {
        let groups = PatchGroups::build(["foo", "foo@>=1.0.0"].into_iter()).unwrap();
        // A git/exotic version can't satisfy the range but still hits `all`.
        assert_eq!(
            groups.resolve("foo", "git-sha-nonsemver").unwrap(),
            Some("foo")
        );
    }

    fn mk_pkg(name: &str, version: &str) -> crate::LockedPackage {
        crate::LockedPackage {
            name: name.into(),
            version: version.into(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_by_version_maps_source_keys_to_concrete_package_versions() {
        let mut packages = BTreeMap::new();
        packages.insert("foo@1.0.0".to_string(), mk_pkg("foo", "1.0.0"));
        // A peer-context variant shares one spec_key and must not duplicate.
        packages.insert(
            "foo@1.0.0(react@18.2.0)".to_string(),
            mk_pkg("foo", "1.0.0"),
        );
        packages.insert("foo@2.0.0".to_string(), mk_pkg("foo", "2.0.0"));
        packages.insert("bar@3.1.0".to_string(), mk_pkg("bar", "3.1.0"));

        // Bare `foo` patches every foo; range `bar@>=3` patches bar@3.1.0.
        let source: BTreeMap<String, String> = [
            ("foo".to_string(), "patches/foo.patch".to_string()),
            ("bar@>=3.0.0".to_string(), "patches/bar.patch".to_string()),
        ]
        .into();

        let resolved = resolve_patched_by_version(&source, &packages).unwrap();
        assert_eq!(
            resolved.get("foo@1.0.0").map(String::as_str),
            Some("patches/foo.patch")
        );
        assert_eq!(
            resolved.get("foo@2.0.0").map(String::as_str),
            Some("patches/foo.patch")
        );
        assert_eq!(
            resolved.get("bar@3.1.0").map(String::as_str),
            Some("patches/bar.patch")
        );
        assert_eq!(
            resolved.len(),
            3,
            "peer-context variant must collapse to one entry"
        );
    }

    fn mk_alias(alias: &str, registry_name: &str, version: &str) -> crate::LockedPackage {
        crate::LockedPackage {
            name: alias.into(),
            version: version.into(),
            alias_of: Some(registry_name.into()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_by_version_aliased_package_inherits_registry_name_patch() {
        let mut packages = BTreeMap::new();
        packages.insert("is-odd@3.0.1".to_string(), mk_pkg("is-odd", "3.0.1"));
        packages.insert(
            "odd-alias@3.0.1".to_string(),
            mk_alias("odd-alias", "is-odd", "3.0.1"),
        );
        let source: BTreeMap<String, String> = [(
            "is-odd@3.0.1".to_string(),
            "patches/is-odd@3.0.1.patch".to_string(),
        )]
        .into();

        let resolved = resolve_patched_by_version(&source, &packages).unwrap();
        // The alias entry is keyed by the ALIAS spec_key — what the
        // linker's `Patches` map and the graph-hash fold look up — not by
        // the registry name the selector was written against.
        assert_eq!(
            resolved.get("odd-alias@3.0.1").map(String::as_str),
            Some("patches/is-odd@3.0.1.patch"),
            "an aliased install must inherit its registry name's patch (pnpm has no separate alias node)"
        );
        assert_eq!(
            resolved.get("is-odd@3.0.1").map(String::as_str),
            Some("patches/is-odd@3.0.1.patch")
        );
    }

    #[test]
    fn resolve_by_version_ignores_a_key_naming_the_alias_and_reports_it() {
        let mut packages = BTreeMap::new();
        packages.insert(
            "odd-alias@3.0.1".to_string(),
            mk_alias("odd-alias", "is-odd", "3.0.1"),
        );
        let source: BTreeMap<String, String> =
            [("odd-alias@3.0.1".to_string(), "patches/x.patch".to_string())].into();

        // pnpm resolves on the registry name, so nothing is named
        // `odd-alias` and the key patches nothing.
        assert!(
            resolve_patched_by_version(&source, &packages)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            unused_patch_keys(source.keys().map(String::as_str), &packages).unwrap(),
            vec![UnusedPatchKey {
                source_key: "odd-alias@3.0.1",
                registry_name_spelling: Some("is-odd@3.0.1".to_string()),
            }],
            "dead alias-name config must be reportable, with the spelling that works"
        );
    }

    #[test]
    fn unused_patch_keys_reports_only_the_dead_key_beside_an_applying_one() {
        let mut packages = BTreeMap::new();
        packages.insert(
            "odd-alias@3.0.1".to_string(),
            mk_alias("odd-alias", "is-odd", "3.0.1"),
        );
        // `is-odd@3.0.1` patches the aliased install and must never be
        // reported, or every project the alias fix serves gets flagged.
        // `ghost@1.0.0` matches nothing — and an unused key sitting
        // beside one that applies still counts (pnpm fails there too).
        let source: BTreeMap<String, String> = [
            (
                "is-odd@3.0.1".to_string(),
                "patches/is-odd@3.0.1.patch".to_string(),
            ),
            ("ghost@1.0.0".to_string(), "patches/ghost.patch".to_string()),
        ]
        .into();

        assert_eq!(
            unused_patch_keys(source.keys().map(String::as_str), &packages).unwrap(),
            vec![UnusedPatchKey {
                source_key: "ghost@1.0.0",
                registry_name_spelling: None,
            }]
        );
    }

    #[test]
    fn resolve_by_version_propagates_conflict() {
        let mut packages = BTreeMap::new();
        packages.insert("foo@3.1.0".to_string(), mk_pkg("foo", "3.1.0"));
        let source: BTreeMap<String, String> = [
            ("foo@>=3.0.0".to_string(), "a".to_string()),
            ("foo@^3.1.0".to_string(), "b".to_string()),
        ]
        .into();
        assert!(matches!(
            resolve_patched_by_version(&source, &packages),
            Err(PatchResolveError::Conflict(_))
        ));
    }
}
