/// Return the graph the lockfile writer should serialize, honoring the
/// `persist_times` gate.
///
/// The resolver keeps publish times in `graph.times` whenever any of
/// time-based mode / `minimumReleaseAge` / `trustPolicy=no-downgrade`
/// is active (the in-memory map feeds the cutoff computation and the
/// embedder's `defaultTrust` floor). pnpm, however, persists a
/// top-level `time:` block to the lockfile *only* under
/// `resolution-mode=time-based`. This decouples the two: when
/// `persist_times` is false (any non-time-based mode), the writer sees a
/// `times`-free clone so the lockfile stays byte-for-byte pnpm-parity,
/// while the live in-memory graph (which the floor clones) keeps its
/// times intact.
///
/// Returns a borrow when `persist_times` is true (the common
/// time-based path and the no-times-recorded path both write the graph
/// unchanged) and an owned `times`-stripped clone otherwise.
pub(super) fn lockfile_graph_for_write(
    graph: &aube_lockfile::LockfileGraph,
    persist_times: bool,
) -> std::borrow::Cow<'_, aube_lockfile::LockfileGraph> {
    if persist_times || graph.times.is_empty() {
        std::borrow::Cow::Borrowed(graph)
    } else {
        let mut stripped = graph.clone();
        stripped.times.clear();
        std::borrow::Cow::Owned(stripped)
    }
}

fn remap_lockfile_importer(graph: &mut aube_lockfile::LockfileGraph, importer_key: &str) {
    if importer_key != "."
        && let Some(deps) = graph.importers.remove(importer_key)
    {
        graph.importers.insert(".".to_string(), deps);
    }
}

/// Read a lockfile from `lockfile_dir`, preserve the detected kind,
/// and remap its importer key for the current project from the
/// project's relative-path key to `"."`. No-op when
/// `importer_key == "."`.
pub(super) fn parse_lockfile_dir_remapped_with_kind_and_options(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    manifest: &aube_manifest::PackageJson,
    options: aube_lockfile::ParseOptions,
) -> Result<(aube_lockfile::LockfileGraph, aube_lockfile::LockfileKind), aube_lockfile::Error> {
    let (mut graph, kind) =
        aube_lockfile::parse_lockfile_with_kind_and_options(lockfile_dir, manifest, options)?;
    remap_lockfile_importer(&mut graph, importer_key);
    Ok((graph, kind))
}

/// Refuse to operate on a `--lockfile-dir` lockfile that already
/// records other importers besides the current project. This PR
/// scopes `--lockfile-dir` to single-project relocation; multi-
/// project shared lockfiles need workspace coordination (resolve
/// every importer's deps in one pass, prune packages by union of all
/// importers) which is out of scope. Without this guard, a second
/// project pointed at the same dir would silently orphan-strip the
/// first project's package entries on the next install. Loud-fail
/// here so the user can move to a workspace setup or pick a
/// different `lockfileDir`.
pub(super) fn guard_against_foreign_importers(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<(), aube_lockfile::Error> {
    // Caller gates on `importer_key != "."`, so any `"."` entry on
    // disk is itself a project that ran `aube install` directly in
    // `lockfile_dir` without `--lockfile-dir`. That entry would be
    // dropped on write, so it counts as foreign.
    let foreign: Vec<&str> = graph
        .importers
        .keys()
        .map(String::as_str)
        .filter(|k| *k != importer_key)
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    Err(aube_lockfile::Error::Parse(
        lockfile_dir.to_path_buf(),
        format!(
            "lockfile already records importers from other projects ({}); \
             aube does not yet support multi-project shared lockfiles outside a workspace. \
             Use a `pnpm-workspace.yaml` workspace, or point each project at its own `--lockfile-dir`.",
            foreign.join(", ")
        ),
    ))
}

/// Write `graph` to `lockfile_dir`, remapping the project's `"."`
/// importer key to its relative-path key from `lockfile_dir`.
/// No-op remap when `importer_key == "."`.
pub(super) fn write_lockfile_dir_remapped(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    kind: aube_lockfile::LockfileKind,
) -> Result<std::path::PathBuf, aube_lockfile::Error> {
    if importer_key == "." {
        return aube_lockfile::write_lockfile_as(lockfile_dir, graph, manifest, kind);
    }
    let mut remapped = graph.clone();
    let deps = remapped.importers.remove(".").ok_or_else(|| {
        aube_lockfile::Error::Parse(
            lockfile_dir.to_path_buf(),
            format!(
                "in-memory lockfile graph missing `.` importer; cannot write under key `{importer_key}`"
            ),
        )
    })?;
    remapped.importers.insert(importer_key.to_string(), deps);
    aube_lockfile::write_lockfile_as(lockfile_dir, &remapped, manifest, kind)
}

#[cfg(test)]
mod tests {
    use super::lockfile_graph_for_write;
    use std::borrow::Cow;

    /// The resolver keeps publish times in `graph.times` whenever
    /// minimumReleaseAge / trustPolicy / the defaultTrust floor is active,
    /// but pnpm serializes a `time:` block only under time-based
    /// resolution. Every lockfile write — the main write and the catch-up
    /// integrity rewrite — routes through this helper, so pinning its two
    /// branches guards the #520 parity leak (the rewrite path once wrote
    /// its own un-stripped clone, re-introducing `time:` on a
    /// non-time-based lockfile).
    #[test]
    fn strips_times_only_when_not_persisting() {
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .times
            .insert("foo@1.0.0".into(), "2020-01-01T00:00:00.000Z".into());

        // Non-time-based: the writer's view is `time:`-free, and the strip
        // is on a clone — the shared graph keeps its times for the floor.
        let stripped = lockfile_graph_for_write(&graph, false);
        assert!(stripped.times.is_empty());
        assert!(matches!(stripped, Cow::Owned(_)));
        assert!(!graph.times.is_empty());

        // Time-based: times persist, no clone made.
        let kept = lockfile_graph_for_write(&graph, true);
        assert_eq!(kept.times.len(), 1);
        assert!(matches!(kept, Cow::Borrowed(_)));
    }
}
