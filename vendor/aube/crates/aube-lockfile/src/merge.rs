//! Branch-lockfile merge.
//!
//! Implements pnpm's `merge-git-branch-lockfiles` workflow for aube.
//! When `gitBranchLockfile: true` is set, each branch writes its
//! lockfile to `aube-lock.<branch>.yaml`. When the user lands on a
//! collapse branch (e.g. `main` or `release/*`, configured via
//! `mergeGitBranchLockfilesBranchPattern`), or when they pass
//! `--merge-git-branch-lockfiles`, aube globs the branch-specific
//! files, unions their package graphs into `aube-lock.yaml`, and
//! deletes the branch files.
//!
//! Conflict rule: when two branch files record the same `dep_path`
//! with different `version`/`integrity`, the entry whose `version`
//! parses as the higher semver wins and a warning is logged to
//! `tracing`. Stable tie-breaking: equal semver keeps the base file
//! value (or the first branch file in sorted-filename order).

use crate::{DirectDep, LockfileGraph, pnpm};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Summary of one merge pass, surfaced to callers so they can log it
/// through their preferred UI (aube uses `progress::println`).
#[derive(Debug, Default, Clone)]
pub struct MergeReport {
    /// Branch-lockfile paths that were parsed and merged, then deleted.
    pub merged_files: Vec<PathBuf>,
    /// `dep_path`s where two branch files recorded different
    /// `integrity` or `version`. Populated with the message for each
    /// conflict; the actual resolution is already applied to the
    /// merged graph.
    pub conflicts: Vec<String>,
}

/// Glob all `aube-lock.*.yaml` files in `project_dir` (excluding plain
/// `aube-lock.yaml`), parse each, merge them into the base
/// `aube-lock.yaml` (or an empty graph if no base exists), write the
/// merged result, and delete each successfully-merged branch file.
///
/// Returns a [`MergeReport`] describing what happened. If no branch
/// files are found, the report is empty and no files are written.
pub fn merge_branch_lockfiles(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<MergeReport, crate::Error> {
    let mut report = MergeReport::default();

    let branch_paths = discover_branch_lockfiles(project_dir);
    if branch_paths.is_empty() {
        return Ok(report);
    }

    let base_path = project_dir.join(aube_util::embedder().lockfile_basename);
    let mut merged = if base_path.exists() {
        pnpm::parse(&base_path)?
    } else {
        LockfileGraph::default()
    };

    // Sorted-filename order gives deterministic output. Parse first,
    // then delete — a parse failure on any file aborts the whole
    // merge and leaves every file in place.
    let mut parsed: Vec<(PathBuf, LockfileGraph)> = Vec::with_capacity(branch_paths.len());
    for path in &branch_paths {
        let graph = pnpm::parse(path)?;
        parsed.push((path.clone(), graph));
    }

    for (path, graph) in parsed {
        merge_into(&mut merged, graph, &mut report);
        report.merged_files.push(path);
    }

    // Write out the combined graph as `aube-lock.yaml` (plain filename,
    // not branch-scoped).
    pnpm::write(&base_path, &merged, manifest)?;

    for path in &report.merged_files {
        if let Err(err) = std::fs::remove_file(path) {
            // Non-fatal: the merged graph is already written. Surface
            // a warning so the user can clean up manually if needed.
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_LOCKFILE_MERGE_CLEANUP_FAILED,
                "failed to remove merged branch lockfile {}: {err}",
                path.display()
            );
        }
    }

    Ok(report)
}

/// Return whether the current git branch (if any) matches the
/// user-provided pattern list. A match occurs when *any* positive
/// pattern matches AND *no* negative (`!`-prefixed) pattern matches.
/// Returns `false` if we can't determine a branch (no git, detached
/// HEAD, etc.) or the pattern list is empty.
pub fn current_branch_matches(project_dir: &Path, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let Some(branch) = crate::current_git_branch(project_dir) else {
        return false;
    };
    branch_matches_patterns(&branch, patterns)
}

/// Pattern-matching logic split out so we can unit-test it without a
/// real git repo.
fn branch_matches_patterns(branch: &str, patterns: &[String]) -> bool {
    let mut any_positive = false;
    let mut any_positive_match = false;
    for raw in patterns {
        if let Some(neg) = raw.strip_prefix('!') {
            if let Ok(pat) = glob::Pattern::new(neg)
                && pat.matches(branch)
            {
                // Explicit negation wins.
                return false;
            }
        } else {
            any_positive = true;
            if let Ok(pat) = glob::Pattern::new(raw)
                && pat.matches(branch)
            {
                any_positive_match = true;
            }
        }
    }
    // "Only negations" (no positives) is treated as no match, matching
    // pnpm's behavior — the setting is opt-in, so at least one
    // positive pattern is required to enable merging.
    any_positive && any_positive_match
}

fn discover_branch_lockfiles(project_dir: &Path) -> Vec<PathBuf> {
    // `glob` needs a string pattern. Project dirs with non-UTF-8
    // segments can't be matched; fall back to empty (aube doesn't
    // support non-UTF-8 project roots elsewhere either).
    let Some(dir_str) = project_dir.to_str() else {
        return Vec::new();
    };
    let basename = aube_util::embedder().lockfile_basename;
    let (stem, ext) = basename.rsplit_once('.').unwrap_or((basename, "yaml"));
    let pattern = format!("{dir_str}/{stem}.*.{ext}");
    let mut out: Vec<PathBuf> = glob::glob(&pattern)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|p| {
            // `<stem>.*.<ext>` also matches the plain canonical lockfile
            // itself on some implementations; filter it out explicitly.
            p.file_name().and_then(|n| n.to_str()) != Some(basename)
        })
        .collect();
    out.sort();
    out
}

/// Merge `src` into `dst`. Conflicts on `packages` are recorded into
/// `report.conflicts` and resolved by keeping the higher-semver entry.
fn merge_into(dst: &mut LockfileGraph, src: LockfileGraph, report: &mut MergeReport) {
    // Packages: same `dep_path` with different `version` or `integrity`
    // is a conflict; resolve by higher semver.
    for (dep_path, incoming) in src.packages {
        match dst.packages.remove(&dep_path) {
            Some(existing) => {
                let version_diff = existing.version != incoming.version;
                let integrity_diff = existing.integrity != incoming.integrity;
                if version_diff || integrity_diff {
                    // Integrity mismatch on same version is a real
                    // supply chain signal. Means one branch fetched
                    // a tarball with different bytes than the other.
                    // Registry re-publish, mirror replay, or worse.
                    // Flag it LOUDER than a plain version conflict
                    // so the user actually investigates instead of
                    // just accepting "higher semver wins" silently.
                    let keep_existing = prefer_higher_version(&existing.version, &incoming.version);
                    let chosen = if keep_existing { existing } else { incoming };
                    let reason = if !version_diff && integrity_diff {
                        format!(
                            "INTEGRITY MISMATCH on same version {} (one branch may have \
                             a tampered or re-published tarball, investigate before \
                             trusting the merged lockfile)",
                            chosen.version
                        )
                    } else if version_diff && integrity_diff {
                        format!(
                            "version and integrity both differ, kept version {}",
                            chosen.version
                        )
                    } else {
                        format!("version differs, kept {}", chosen.version)
                    };
                    report.conflicts.push(format!("{dep_path}: {reason}"));
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_LOCKFILE_MERGE_CONFLICT,
                        "merge conflict on {dep_path}: {reason}"
                    );
                    dst.packages.insert(dep_path, chosen);
                } else {
                    // Identical. Put existing one back.
                    dst.packages.insert(dep_path, existing);
                }
            }
            None => {
                dst.packages.insert(dep_path, incoming);
            }
        }
    }

    // Importers: union by importer key. Same DirectDep name → keep the
    // one whose `dep_path` sorts higher by semver; same DirectDep name
    // with identical dep_path is a no-op.
    for (importer_key, incoming_deps) in src.importers {
        let entry = dst.importers.entry(importer_key.clone()).or_default();
        merge_direct_deps(entry, incoming_deps, &importer_key, report);
    }

    // Overrides / ignored / skipped / times / catalogs: union where
    // straightforward. Preserve base's `settings` header to keep
    // round-trip stability (the primary lockfile is authoritative for
    // header fields like `auto_install_peers`).
    for (k, v) in src.overrides {
        // Old code was or_insert which silently picked base on a
        // collision. User intent divergence dropped without a
        // peep. Now record the conflict when values differ, still
        // pick base for determinism but tell the user the other
        // branch wanted something else.
        use std::collections::btree_map::Entry;
        match dst.overrides.entry(k) {
            Entry::Vacant(slot) => {
                slot.insert(v);
            }
            Entry::Occupied(slot) => {
                if slot.get() != &v {
                    report.conflicts.push(format!(
                        "override `{}`: kept {} over {}",
                        slot.key(),
                        slot.get(),
                        v
                    ));
                }
            }
        }
    }
    for name in src.ignored_optional_dependencies {
        dst.ignored_optional_dependencies.insert(name);
    }
    for (key, incoming) in src.patched_dependencies {
        use std::collections::btree_map::Entry;
        // The hash sidecar travels with its path entry: whichever
        // side's path wins keeps that side's hash, so a `{hash, path}`
        // pair never ends up mixed across branches.
        let incoming_hash = src.patched_dependency_hashes.get(&key).cloned();
        match dst.patched_dependencies.entry(key) {
            Entry::Vacant(slot) => {
                if let Some(hash) = incoming_hash {
                    dst.patched_dependency_hashes
                        .insert(slot.key().clone(), hash);
                }
                slot.insert(incoming);
            }
            Entry::Occupied(slot) => {
                if slot.get() != &incoming {
                    report.conflicts.push(format!(
                        "patched dependency `{}`: kept {} over {}",
                        slot.key(),
                        slot.get(),
                        incoming
                    ));
                } else if let Some(hash) = incoming_hash {
                    dst.patched_dependency_hashes
                        .entry(slot.key().clone())
                        .or_insert(hash);
                }
            }
        }
    }
    for (name, incoming) in src.runtimes {
        use std::collections::btree_map::Entry;
        match dst.runtimes.entry(name) {
            Entry::Vacant(slot) => {
                slot.insert(incoming);
            }
            Entry::Occupied(mut slot) => {
                if slot.get().version != incoming.version {
                    report.conflicts.push(format!(
                        "runtime `{}`: kept {} over {}",
                        slot.key(),
                        slot.get().version,
                        incoming.version
                    ));
                    continue;
                }
                // Same resolved version: a silently-kept dst would
                // drop the incoming side's metadata. Surface specifier
                // disagreements and union the per-platform variants
                // (dst wins on target collisions) so a pin written on
                // another platform keeps its artifacts after a merge.
                if slot.get().specifier != incoming.specifier {
                    report.conflicts.push(format!(
                        "runtime `{}` specifier: kept {} over {}",
                        slot.key(),
                        slot.get().specifier,
                        incoming.specifier
                    ));
                }
                let dst_pin = slot.get_mut();
                for variant in incoming.variants {
                    // Push unless *every* target is already covered —
                    // `.any()` here would drop a multi-target variant
                    // whose targets are only partially covered.
                    let fully_covered = variant.targets.iter().all(|t| {
                        dst_pin
                            .variants
                            .iter()
                            .any(|dv| dv.targets.iter().any(|dt| dt == t))
                    });
                    if !fully_covered {
                        dst_pin.variants.push(variant);
                    }
                }
            }
        }
    }
    let mut seen: aube_util::collections::FxSet<String> =
        dst.trusted_dependencies.iter().cloned().collect();
    for name in src.trusted_dependencies {
        if seen.insert(name.clone()) {
            dst.trusted_dependencies.push(name);
        }
    }
    for (importer_key, entries) in src.skipped_optional_dependencies {
        let merged = dst
            .skipped_optional_dependencies
            .entry(importer_key)
            .or_default();
        for (name, spec) in entries {
            merged.entry(name).or_insert(spec);
        }
    }
    for (key, incoming_time) in src.times {
        // Prefer the lexicographically-larger ISO-8601 timestamp —
        // matches "latest wins" without parsing.
        dst.times
            .entry(key)
            .and_modify(|existing| {
                if incoming_time > *existing {
                    *existing = incoming_time.clone();
                }
            })
            .or_insert(incoming_time);
    }
    for (cat_name, entries) in src.catalogs {
        // Catalog merge used to be silent first-write-wins. Two
        // branches bumping the same catalog pin (`react: ^18` vs
        // `^19`) left base untouched with zero user feedback.
        // Catalog drift is a root cause of "works on my branch,
        // fails in CI" since the bumped version never reaches the
        // merged lockfile. Record conflicts now.
        use std::collections::btree_map::Entry;
        let cat_label = cat_name.clone();
        let merged = dst.catalogs.entry(cat_name).or_default();
        for (name, entry) in entries {
            match merged.entry(name) {
                Entry::Vacant(slot) => {
                    slot.insert(entry);
                }
                Entry::Occupied(slot) => {
                    if slot.get().specifier != entry.specifier {
                        report.conflicts.push(format!(
                            "catalog `{}` entry `{}`: kept {} over {}",
                            cat_label,
                            slot.key(),
                            slot.get().specifier,
                            entry.specifier
                        ));
                    }
                }
            }
        }
    }
}

fn merge_direct_deps(
    dst: &mut Vec<DirectDep>,
    incoming: Vec<DirectDep>,
    importer_key: &str,
    report: &mut MergeReport,
) {
    let mut by_name: BTreeMap<String, DirectDep> =
        dst.drain(..).map(|d| (d.name.clone(), d)).collect();
    for dep in incoming {
        match by_name.remove(&dep.name) {
            Some(existing) => {
                // Record the conflict when the user's declared
                // range differs between branches. Old code picked
                // the entry with the higher resolved dep_path
                // version silently, which overwrote the user's
                // manifest intent. If branch-A had "^1" and
                // branch-B had "^2", branch-B won but the user on
                // branch-A never learned their pin got clobbered
                // on merge.
                if existing.specifier != dep.specifier {
                    let importer_label = if importer_key.is_empty() {
                        "<root>".to_string()
                    } else {
                        importer_key.to_string()
                    };
                    let a = existing.specifier.as_deref().unwrap_or("<none>");
                    let b = dep.specifier.as_deref().unwrap_or("<none>");
                    report.conflicts.push(format!(
                        "importer `{importer_label}` dep `{}`: branches disagreed on \
                         specifier ({a} vs {b}), kept the one resolving to higher version",
                        dep.name
                    ));
                }
                let keep_existing = prefer_higher_version(
                    existing_version_from_dep_path(&existing),
                    existing_version_from_dep_path(&dep),
                );
                by_name.insert(dep.name.clone(), if keep_existing { existing } else { dep });
            }
            None => {
                by_name.insert(dep.name.clone(), dep);
            }
        }
    }
    dst.extend(by_name.into_values());
}

/// Extract the canonical `version` portion from a DirectDep's
/// `dep_path`. Used purely for "higher wins" tie-breaking during
/// direct-dep merging, so a best-effort parse is fine.
fn existing_version_from_dep_path(dep: &DirectDep) -> &str {
    // dep_path shape: `name@version` possibly followed by `(peer)...`
    // or `_<hashed-suffix>`. We want just the `version` portion
    // between the last `@` (there may be two for scoped packages) and
    // any trailing peer/hash marker.
    let after_at = match dep
        .dep_path
        .strip_prefix(&format!("{}@", dep.name))
        .or_else(|| dep.dep_path.rsplit_once('@').map(|(_, v)| v))
    {
        Some(rest) => rest,
        None => return &dep.dep_path,
    };
    // Strip peer suffix first (everything from the first `(`), then
    // the hashed marker if present.
    let without_peer = after_at.split_once('(').map(|(v, _)| v).unwrap_or(after_at);
    without_peer
        .split_once('_')
        .map(|(v, _)| v)
        .unwrap_or(without_peer)
}

/// Return `true` if `a` should be preferred over `b` (i.e. keep the
/// existing entry). Uses semver comparison; unparseable versions
/// fall back to string comparison so behavior is deterministic and
/// never panics.
fn prefer_higher_version(a: &str, b: &str) -> bool {
    match (
        node_semver::Version::parse(a),
        node_semver::Version::parse(b),
    ) {
        (Ok(va), Ok(vb)) => va >= vb,
        _ => a >= b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LockedPackage;

    #[test]
    fn branch_matches_patterns_basic() {
        let patterns = vec!["main".to_string(), "release/*".to_string()];
        assert!(branch_matches_patterns("main", &patterns));
        assert!(branch_matches_patterns("release/v1", &patterns));
        assert!(!branch_matches_patterns("feature/x", &patterns));
    }

    #[test]
    fn branch_matches_patterns_negation_wins() {
        let patterns = vec![
            "main".to_string(),
            "release/*".to_string(),
            "!release/legacy-*".to_string(),
        ];
        assert!(branch_matches_patterns("release/v1", &patterns));
        assert!(!branch_matches_patterns("release/legacy-v0", &patterns));
        assert!(branch_matches_patterns("main", &patterns));
    }

    #[test]
    fn branch_matches_patterns_only_negations_is_false() {
        // A list with only `!x` patterns means "never merge" — matches
        // pnpm, which requires at least one positive pattern.
        let patterns = vec!["!feature/*".to_string()];
        assert!(!branch_matches_patterns("main", &patterns));
        assert!(!branch_matches_patterns("feature/x", &patterns));
    }

    #[test]
    fn branch_matches_patterns_empty_is_false() {
        assert!(!branch_matches_patterns("main", &[]));
    }

    #[test]
    fn existing_version_from_dep_path_handles_forms() {
        let plain = DirectDep {
            name: "react".into(),
            dep_path: "react@18.2.0".into(),
            dep_type: crate::DepType::Production,
            specifier: None,
        };
        assert_eq!(existing_version_from_dep_path(&plain), "18.2.0");

        let nested = DirectDep {
            name: "react-dom".into(),
            dep_path: "react-dom@18.2.0(react@18.2.0)".into(),
            dep_type: crate::DepType::Production,
            specifier: None,
        };
        assert_eq!(existing_version_from_dep_path(&nested), "18.2.0");

        let hashed = DirectDep {
            name: "huge".into(),
            dep_path: "huge@1.0.0_abcdef0123".into(),
            dep_type: crate::DepType::Production,
            specifier: None,
        };
        assert_eq!(existing_version_from_dep_path(&hashed), "1.0.0");
    }

    #[test]
    fn merge_into_unions_disjoint_packages() {
        let mut dst = LockfileGraph::default();
        dst.packages.insert(
            "a@1.0.0".into(),
            LockedPackage {
                name: "a".into(),
                version: "1.0.0".into(),
                ..Default::default()
            },
        );
        let mut src = LockfileGraph::default();
        src.packages.insert(
            "b@2.0.0".into(),
            LockedPackage {
                name: "b".into(),
                version: "2.0.0".into(),
                ..Default::default()
            },
        );
        let mut report = MergeReport::default();
        merge_into(&mut dst, src, &mut report);
        assert!(dst.packages.contains_key("a@1.0.0"));
        assert!(dst.packages.contains_key("b@2.0.0"));
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn merge_into_picks_higher_version_on_conflict() {
        let mut dst = LockfileGraph::default();
        dst.packages.insert(
            "pkg@1.0.0".into(),
            LockedPackage {
                name: "pkg".into(),
                version: "1.0.0".into(),
                integrity: Some("sha512-aaa".into()),
                ..Default::default()
            },
        );
        let mut src = LockfileGraph::default();
        // Same dep_path key, different version + integrity.
        src.packages.insert(
            "pkg@1.0.0".into(),
            LockedPackage {
                name: "pkg".into(),
                version: "2.0.0".into(),
                integrity: Some("sha512-bbb".into()),
                ..Default::default()
            },
        );
        let mut report = MergeReport::default();
        merge_into(&mut dst, src, &mut report);
        assert_eq!(dst.packages["pkg@1.0.0"].version, "2.0.0");
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.conflicts[0].contains("2.0.0"));
    }

    #[test]
    fn prefer_higher_version_semver_order() {
        assert!(prefer_higher_version("2.0.0", "1.0.0"));
        assert!(!prefer_higher_version("1.0.0", "2.0.0"));
        // Fallback: string compare for non-semver tails.
        assert!(prefer_higher_version("workspace:z", "workspace:a"));
    }

    #[test]
    fn merge_into_preserves_patched_dependencies() {
        let mut dst = LockfileGraph::default();
        let mut src = LockfileGraph::default();
        src.patched_dependencies.insert(
            "lodash@4.17.21".into(),
            "patches/lodash@4.17.21.patch".into(),
        );
        let mut report = MergeReport::default();
        merge_into(&mut dst, src, &mut report);
        assert!(
            dst.patched_dependencies.contains_key("lodash@4.17.21"),
            "patched_dependencies entry was dropped on merge: {:?}",
            dst.patched_dependencies
        );
    }

    #[test]
    fn merge_into_preserves_trusted_dependencies() {
        let mut dst = LockfileGraph::default();
        let mut src = LockfileGraph::default();
        src.trusted_dependencies.push("esbuild".into());
        let mut report = MergeReport::default();
        merge_into(&mut dst, src, &mut report);
        assert!(
            dst.trusted_dependencies.iter().any(|n| n == "esbuild"),
            "trusted_dependencies was dropped on merge: {:?}",
            dst.trusted_dependencies
        );
    }
}
