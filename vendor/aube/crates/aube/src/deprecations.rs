//! Shared deprecation-warning plumbing for install and `aube deprecations`.
//!
//! The resolver stashes a deprecation message on each [`ResolvedPackage`] it
//! emits; the install command accumulates those into [`DeprecationRecord`]s,
//! classifies them as direct vs. transitive via the [`LockfileGraph`]'s
//! `importers` map, and renders the result according to the user's
//! `deprecationWarnings` setting. The same renderer backs the stand-alone
//! `aube deprecations` command.
//!
//! [`ResolvedPackage`]: aube_resolver::ResolvedPackage
//! [`LockfileGraph`]: aube_lockfile::LockfileGraph

use aube_lockfile::LockfileGraph;
use aube_settings::resolved::DeprecationWarnings;
use clx::style;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DeprecationRecord {
    pub name: String,
    pub version: String,
    pub dep_path: String,
    pub message: Arc<str>,
}

/// Partition records into direct (resolved to a version an importer
/// pins) and transitive. Keying on `(name, version)` derived from
/// each importer's `DirectDep.dep_path` → `LockedPackage` lookup
/// (rather than on `DirectDep.name` or raw `dep_path`) keeps the
/// classification right for npm-aliased entries and for records
/// captured from the fresh-resolve stream, which carry the canonical
/// pre-peer-context `dep_path` while the graph's `dep_path` keys get
/// rewritten by the peer-context pass. A deprecated `foo@2` reached
/// only transitively still falls on the transitive side when an
/// importer pins a non-deprecated `foo@3`. Preserves input order
/// within each bucket.
pub fn classify<'a>(
    records: &'a [DeprecationRecord],
    graph: &LockfileGraph,
) -> (Vec<&'a DeprecationRecord>, Vec<&'a DeprecationRecord>) {
    let direct_keys: BTreeSet<(&str, &str)> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter())
        .filter_map(|d| graph.packages.get(&d.dep_path))
        .map(|pkg| (pkg.name.as_str(), pkg.version.as_str()))
        .collect();
    let mut direct = Vec::new();
    let mut transitive = Vec::new();
    for r in records {
        if direct_keys.contains(&(r.name.as_str(), r.version.as_str())) {
            direct.push(r);
        } else {
            transitive.push(r);
        }
    }
    (direct, transitive)
}

/// Drop records whose `(name, version)` is no longer in the finalized
/// graph (pruned by `filter_graph`'s platform/optional trim). Matches
/// on `(name, version)` — not `dep_path` — because records captured
/// from the fresh-resolve stream predate the resolver's peer-context
/// pass, which rewrites `graph.packages` keys with peer suffixes.
pub fn retain_in_graph(records: &mut Vec<DeprecationRecord>, graph: &LockfileGraph) {
    let present: BTreeSet<(&str, &str)> = graph
        .packages
        .values()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    records.retain(|r| present.contains(&(r.name.as_str(), r.version.as_str())));
}

/// Deduplicate by `(name, version)`. The stream can emit the same canonical
/// package multiple times under different peer-context dep_paths; the user
/// only wants to see each deprecated version once.
pub fn dedupe(records: Vec<DeprecationRecord>) -> Vec<DeprecationRecord> {
    let mut seen: BTreeMap<(String, String), DeprecationRecord> = BTreeMap::new();
    for r in records {
        seen.entry((r.name.clone(), r.version.clone())).or_insert(r);
    }
    seen.into_values().collect()
}

/// Render install-time warnings according to the user's `deprecationWarnings`
/// setting. Writes to stderr. Must be called after the progress UI has been
/// finished (see `InstallProgress::finish`).
pub fn render_install_warnings(
    records: &[DeprecationRecord],
    graph: &LockfileGraph,
    mode: DeprecationWarnings,
) {
    if records.is_empty() {
        return;
    }
    let (direct, transitive) = classify(records, graph);
    match mode {
        DeprecationWarnings::None => {}
        DeprecationWarnings::Summary => write_count_line(records.len(), !transitive.is_empty()),
        DeprecationWarnings::Direct => {
            for r in &direct {
                write_warn_line(r);
            }
            if !transitive.is_empty() {
                write_transitive_count_line(transitive.len());
            }
        }
        DeprecationWarnings::All => {
            for r in direct.iter().chain(transitive.iter()) {
                write_warn_line(r);
            }
        }
    }
}

fn write_warn_line(r: &DeprecationRecord) {
    let line = format!(
        "{} {}@{}: {}",
        style::eyellow("WARN deprecated").bold(),
        r.name,
        r.version,
        r.message
    );
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn write_transitive_count_line(count: usize) {
    let pkgs = pluralizer::pluralize("transitive package", count as isize, true);
    let verb = if count == 1 { "has" } else { "have" };
    // Product name from the active embedder (standalone aube → "aube"), not a
    // literal: this hint streams to stderr mid-install, so it must be branded at
    // the emit site — a host embedder's later output-capture rewrite never sees it.
    let product = aube_util::embedder().name;
    let msg = format!(
        "{pkgs} {verb} deprecation warnings. Run `{product} deprecations --transitive` to see them."
    );
    let _ = writeln!(std::io::stderr(), "{}", style::edim(msg));
}

fn write_count_line(count: usize, has_transitive: bool) {
    let pkgs = pluralizer::pluralize("package", count as isize, true);
    let verb = if count == 1 { "has" } else { "have" };
    // See `write_transitive_count_line`: streamed line, brand at emit.
    let product = aube_util::embedder().name;
    let cmd = if has_transitive {
        format!("{product} deprecations --transitive")
    } else {
        format!("{product} deprecations")
    };
    let msg = format!("{pkgs} {verb} deprecation warnings. Run `{cmd}` to see them.");
    let _ = writeln!(std::io::stderr(), "{}", style::edim(msg));
}
