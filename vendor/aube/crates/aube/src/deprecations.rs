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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::commands::install::InstallOutputLevel;

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
/// setting. Output is routed through the active install control so embedded
/// hosts receive events instead of writes that can collide with their own
/// progress UI.
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
    crate::commands::install::control::output(
        InstallOutputLevel::Warning,
        Some(aube_codes::warnings::WARN_AUBE_DEPRECATED_PACKAGE),
        format!("deprecated {}@{}: {}", r.name, r.version, r.message),
    );
}

fn write_transitive_count_line(count: usize) {
    let pkgs = pluralizer::pluralize("transitive package", count as isize, true);
    let verb = if count == 1 { "has" } else { "have" };
    let msg = append_command_hint(
        format!("{pkgs} {verb} deprecation warnings."),
        &aube_util::cmd("deprecations --transitive"),
    );
    write_summary_line(msg);
}

fn write_count_line(count: usize, has_transitive: bool) {
    let pkgs = pluralizer::pluralize("package", count as isize, true);
    let verb = if count == 1 { "has" } else { "have" };
    let cmd = if has_transitive {
        aube_util::cmd("deprecations --transitive")
    } else {
        aube_util::cmd("deprecations")
    };
    let msg = append_command_hint(format!("{pkgs} {verb} deprecation warnings."), &cmd);
    write_summary_line(msg);
}

// The `deprecations` verb exists under every embedder, so the hint is always
// worth printing; `aube_util::cmd` brands it at the emit site, which is what
// keeps the product name right on a line an embedder's output rewrite may
// never see.
fn append_command_hint(message: String, command: &str) -> String {
    format!("{message} Run `{command}` to see them.")
}

fn write_summary_line(message: String) {
    crate::commands::install::control::output(
        InstallOutputLevel::Warning,
        Some(aube_codes::warnings::WARN_AUBE_DEPRECATED_PACKAGE_SUMMARY),
        message,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::commands::install::{
        InstallControl, InstallEvent, InstallOutputLevel, InstallReporter,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingReporter(Mutex<Vec<InstallEvent>>);

    impl InstallReporter for RecordingReporter {
        fn report(&self, event: InstallEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn command_hint_names_the_command() {
        let message = "1 transitive package has deprecation warnings.".to_string();
        assert_eq!(
            append_command_hint(message, "aube deprecations --transitive"),
            "1 transitive package has deprecation warnings. Run `aube deprecations --transitive` to see them."
        );
    }

    #[tokio::test]
    async fn summary_is_reported_as_a_structured_warning_event() {
        let reporter = Arc::new(RecordingReporter::default());
        let control = InstallControl::events(reporter.clone());

        crate::commands::install::control::scope(control, async {
            write_summary_line("1 package has deprecation warnings.".to_string());
        })
        .await;

        let events = reporter.0.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [InstallEvent::Output {
                level: InstallOutputLevel::Warning,
                code: Some(code),
                message,
            }] if code == aube_codes::warnings::WARN_AUBE_DEPRECATED_PACKAGE_SUMMARY
                && message == "1 package has deprecation warnings."
        ));
    }
}
