//! Overlap the cold-install lockfile write (serialize + reformat +
//! atomic fs write) with the post-write link tail.
//!
//! On a fresh-resolve cold install the lockfile-write block runs as a
//! serial blocking span *before* `filter_graph` + the link phase. On a
//! large tree the serialize + pnpm-parity reformat + atomic write costs
//! 10-55 ms (measured: ~11 ms at 1.5k packages, ~54 ms at 4.2k), all of
//! it on the critical path while the linker sits idle.
//!
//! This moves that work onto a `spawn_blocking` task that runs concurrently
//! with `filter_graph`, the progress reconcile, and `run_link_phase`, then
//! joins before `run_finalize_phase` reads the graph again. The spawned
//! task operates on a clone of the graph (taken after the write-prep
//! mutations, before `filter_graph` mutates the original), so the cold
//! path now clones the graph twice — once here and once for the catch-up
//! integrity-rewrite holder. The extra clone is cheap relative to the
//! work it overlaps: a full graph clone measures ~0.6-3 ms, ~12-18x less
//! than the 11-55 ms write it hides, so the net win is the recovered write
//! time minus one clone (~10 ms at 1.5k packages, ~51 ms at 4.2k).
//!
//! On by default. `AUBE_DISABLE_LOCKFILE_WRITE_OVERLAP=1` reverts to the
//! old inline serial write (used for byte-identity comparison runs and as
//! a fork-discipline killswitch); under that env the caller writes inline
//! at the original point and never spawns a task, so the on-disk result is
//! byte-identical to the pre-overlap code (verified by an `install.bats`
//! cold-install diff).

use super::lockfile_dir::write_lockfile_dir_remapped;
use super::workspace::write_per_project_lockfiles;
use miette::{Context, IntoDiagnostic};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Owned inputs the spawned write task needs. All captures are owned so
/// the closure is `'static`. The graph is the only non-trivial capture (a
/// clone taken at the spawn site); the rest are cheap path/flag copies.
pub(super) struct LockfileWriteInputs {
    pub graph: aube_lockfile::LockfileGraph,
    pub manifest: aube_manifest::PackageJson,
    pub manifests: Vec<(String, aube_manifest::PackageJson)>,
    pub lockfile_dir: PathBuf,
    pub lockfile_importer_key: String,
    pub cwd: PathBuf,
    pub write_kind: aube_lockfile::LockfileKind,
    pub shared_workspace_lockfile: bool,
    pub has_workspace: bool,
    pub per_project_write_selection: Option<BTreeSet<String>>,
}

pub(super) type LockfileWriteHandle = tokio::task::JoinHandle<miette::Result<()>>;

/// Returns whether the write-overlap optimization is enabled (the
/// default). `AUBE_DISABLE_LOCKFILE_WRITE_OVERLAP` (under the active
/// embedder's env prefix) disables it. Reads through `embedder_env` so a
/// host with no `env_prefix` exposes no branded toggle.
pub(super) fn overlap_enabled() -> bool {
    aube_util::env::embedder_env("DISABLE_LOCKFILE_WRITE_OVERLAP").is_none()
}

/// The serialize + reformat + atomic write, with the lockfile graph and
/// every parameter borrowed. Both the overlapped path (via the owned
/// `LockfileWriteInputs`) and the killswitch-disabled inline call site in
/// `mod.rs` call this with the SAME values, so the two produce
/// byte-identical output. This is the exact body the pre-overlap inline
/// write ran.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_one(
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    manifests: &[(String, aube_manifest::PackageJson)],
    lockfile_dir: &std::path::Path,
    lockfile_importer_key: &str,
    cwd: &std::path::Path,
    write_kind: aube_lockfile::LockfileKind,
    shared_workspace_lockfile: bool,
    has_workspace: bool,
    per_project_write_selection: Option<&BTreeSet<String>>,
) -> miette::Result<()> {
    if shared_workspace_lockfile || !has_workspace {
        let written_path = write_lockfile_dir_remapped(
            lockfile_dir,
            lockfile_importer_key,
            graph,
            manifest,
            write_kind,
        )
        .into_diagnostic()
        .wrap_err("failed to write lockfile")?;
        // Matches the format resolve.bats and similar tests assert against
        // (e.g. "Wrote aube-lock.yaml").
        tracing::debug!(
            "Wrote {}",
            written_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| written_path.display().to_string())
        );
    } else {
        write_per_project_lockfiles(
            cwd,
            graph,
            manifests,
            write_kind,
            per_project_write_selection,
        )?;
    }
    Ok(())
}

/// Spawn the lockfile write on a blocking thread so it overlaps the link
/// tail. Takes the owned clone of the graph + the other captures so the
/// closure is `'static`. The returned handle MUST be joined (via [`join`])
/// before the install completes so a write error still surfaces.
pub(super) fn spawn(inputs: LockfileWriteInputs) -> LockfileWriteHandle {
    tokio::task::spawn_blocking(move || {
        let _diag = aube_util::diag::Span::new(
            aube_util::diag::Category::Install,
            "lockfile_write_overlapped",
        );
        write_one(
            &inputs.graph,
            &inputs.manifest,
            &inputs.manifests,
            &inputs.lockfile_dir,
            &inputs.lockfile_importer_key,
            &inputs.cwd,
            inputs.write_kind,
            inputs.shared_workspace_lockfile,
            inputs.has_workspace,
            inputs.per_project_write_selection.as_ref(),
        )
    })
}

/// Join a spawned write handle, mapping a task panic to a diagnostic. A
/// write error surfaces here (at the join point, before finalize) rather
/// than being dropped. Returns the write's own `Result` unchanged so the
/// error chain is exactly what the inline path would have produced, with a
/// panic wrapped distinctly.
pub(super) async fn join(handle: LockfileWriteHandle) -> miette::Result<()> {
    match handle.await {
        Ok(write_result) => write_result,
        // `JoinError` is a plain `std::error::Error` (not a miette
        // `Diagnostic`), so the bridge to a `Report` is `into_diagnostic()`
        // on the `Result`.
        Err(join_err) => Result::<(), _>::Err(join_err)
            .into_diagnostic()
            .wrap_err("lockfile write task panicked"),
    }
}
