//! `aube patch-commit <dir>` — diff a `aube patch` edit directory
//! against its frozen source snapshot, write the unified diff to
//! `<project>/<patches-dir>/<name>@<version>.patch`, record the entry
//! under `pnpm.patchedDependencies` in `package.json`, and re-run
//! install so the patched files land in the linked tree.
//!
//! The patch format is git-compatible: each per-file hunk is wrapped
//! in `diff --git a/<rel> b/<rel>` so a generated patch round-trips
//! through `git apply` as well as aube's own applier in `aube-linker`.

use crate::commands::patch::{PatchState, read_state};
use crate::patches::upsert_patched_dependency;
use clap::Args;
use miette::{IntoDiagnostic, Result, miette};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct PatchCommitArgs {
    /// The edit directory printed by `aube patch`.
    ///
    /// The matching source snapshot is read from a sibling `source/`
    /// dir, located via the `.aube_patch_state.json` sidecar.
    #[arg(value_name = "DIR")]
    pub edit_dir: PathBuf,

    /// Where to write the generated `.patch` file, relative to the
    /// project root.
    ///
    /// Defaults to `patches`.
    #[arg(long, value_name = "DIR", default_value = "patches")]
    pub patches_dir: PathBuf,
}

pub async fn run(args: PatchCommitArgs) -> Result<()> {
    let state = read_state(&args.edit_dir)?;
    let cwd = match state.project.clone() {
        Some(project) => project,
        None => crate::dirs::project_root()?,
    };
    std::env::set_current_dir(&cwd)
        .into_diagnostic()
        .map_err(|e| miette!("failed to change directory to {}: {e}", cwd.display()))?;
    crate::dirs::set_cwd(&cwd)?;

    let patch = build_patch(&state)?;
    if patch.is_empty() {
        return Err(miette!(
            "no changes detected between {} and {}",
            state.source_dir.display(),
            state.user_dir.display()
        ));
    }

    let safe_name = state.name.replace('/', "+");
    let file_name = format!("{safe_name}@{}.patch", state.version);
    let rel_dir = args.patches_dir.clone();
    let abs_dir = cwd.join(&rel_dir);
    std::fs::create_dir_all(&abs_dir)
        .into_diagnostic()
        .map_err(|e| miette!("failed to create {}: {e}", abs_dir.display()))?;
    let abs_path = abs_dir.join(&file_name);
    // Snapshot any existing patch so a re-patch can be rolled back to
    // the previous content if the manifest write fails. atomic_write
    // replaces unconditionally, so without the snapshot a re-patch
    // failure would leave the manifest pointing at a path that lost
    // its old content.
    let prior_patch = std::fs::read(&abs_path).ok();
    aube_util::fs_atomic::atomic_write(&abs_path, patch.as_bytes())
        .into_diagnostic()
        .map_err(|e| miette!("failed to write {}: {e}", abs_path.display()))?;

    // Use forward slashes in the manifest entry. Field is portable
    // across platforms and pnpm always writes it that way.
    let rel_path = format!(
        "{}/{file_name}",
        rel_dir.to_string_lossy().replace('\\', "/")
    );
    let key = format!("{}@{}", state.name, state.version);
    // Manifest write failure means the patch on disk is not
    // referenced anywhere. Restore the prior patch if there was one
    // (re-patch path), else remove the orphan.
    let manifest_path = match upsert_patched_dependency(&cwd, &key, &rel_path) {
        Ok(p) => p,
        Err(e) => {
            match prior_patch {
                Some(bytes) => {
                    let _ = aube_util::fs_atomic::atomic_write(&abs_path, &bytes);
                }
                None => {
                    let _ = std::fs::remove_file(&abs_path);
                }
            }
            return Err(e);
        }
    };

    let manifest_label = manifest_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| manifest_path.display().to_string());
    eprintln!("Wrote {}", abs_path.display());
    eprintln!("Recorded {key} -> {rel_path} in {manifest_label}");

    // Drop the snapshot tempdir now that we've captured the diff —
    // matches pnpm's behavior of cleaning up after a successful commit.
    if let Some(parent) = state.user_dir.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }

    // Re-run install so the new patch is applied. We deliberately
    // avoid touching the lockfile here — the patch only changes
    // file contents, not the resolved graph.
    let opts = crate::commands::install::InstallOptions::with_mode(
        crate::commands::install::FrozenMode::Prefer,
    );
    crate::commands::install::run(opts).await?;

    Ok(())
}

/// Walk the source and user dirs in lockstep, emitting a unified
/// diff for every file that differs (including pure additions and
/// deletions). The output is a single concatenated git-style patch.
fn build_patch(state: &PatchState) -> Result<String> {
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    collect_files(&state.source_dir, &state.source_dir, &mut files)?;
    collect_files(&state.user_dir, &state.user_dir, &mut files)?;

    let mut out = String::new();
    for rel in files {
        let src = state.source_dir.join(&rel);
        let dst = state.user_dir.join(&rel);
        let (Some(src_text), Some(dst_text)) = (read_or_empty(&src)?, read_or_empty(&dst)?) else {
            // Either side is binary — fall back to a byte comparison
            // and warn if it changed. We can't emit a unified diff for
            // binary content, but we also don't want to abort the whole
            // commit just because the package ships a `.node` addon.
            let src_bytes = std::fs::read(&src).unwrap_or_default();
            let dst_bytes = std::fs::read(&dst).unwrap_or_default();
            if src_bytes != dst_bytes {
                eprintln!(
                    "warning: {} differs but is binary — skipping (aube can't diff binary files)",
                    rel.display()
                );
            }
            continue;
        };
        if src_text == dst_text {
            continue;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let patch = diffy::create_patch(&src_text, &dst_text);
        // Strip diffy's default `--- original` / `+++ modified` lines
        // and replace them with `a/<rel>` / `b/<rel>` so the result
        // matches git's format. Do it textually because diffy's API
        // doesn't expose header rewriting directly.
        let body = patch.to_string();
        let body = strip_default_headers(&body);
        // Match git's convention for added/deleted files: `--- /dev/null`
        // for an addition, `+++ /dev/null` for a deletion. The linker
        // recognizes these markers and creates / removes the file rather
        // than writing an empty placeholder.
        let src_missing = !src.exists();
        let dst_missing = !dst.exists();
        let from_header = if src_missing {
            "--- /dev/null".to_string()
        } else {
            format!("--- a/{rel_str}")
        };
        let to_header = if dst_missing {
            "+++ /dev/null".to_string()
        } else {
            format!("+++ b/{rel_str}")
        };
        out.push_str(&format!("diff --git a/{rel_str} b/{rel_str}\n"));
        out.push_str(&from_header);
        out.push('\n');
        out.push_str(&to_header);
        out.push('\n');
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Read a file as UTF-8 text. Returns `Ok(Some(""))` for missing files
/// (so additions and deletions diff cleanly against an empty side),
/// `Ok(None)` for binary files (which the caller falls back to a byte
/// comparison + warning), and a hard error only for genuine I/O
/// failures the user should see.
fn read_or_empty(p: &Path) -> Result<Option<String>> {
    if !p.exists() {
        return Ok(Some(String::new()));
    }
    match std::fs::read_to_string(p) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Ok(None),
        Err(e) => Err(miette!("failed to read {}: {e}", p.display())),
    }
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", dir.display()))?
    {
        let entry = entry
            .into_diagnostic()
            .map_err(|e| miette!("failed to read entry: {e}"))?;
        let path = entry.path();
        let ty = entry
            .file_type()
            .into_diagnostic()
            .map_err(|e| miette!("failed to stat {}: {e}", path.display()))?;
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            // Skip nested node_modules — packages may install their
            // own deps for ergonomics, and we don't want to drag those
            // into the patch.
            if path.file_name().is_some_and(|n| n == "node_modules") {
                continue;
            }
            collect_files(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.insert(rel.to_path_buf());
        }
    }
    Ok(())
}

/// Drop diffy's first two header lines (`--- original\n+++ modified\n`)
/// so we can prepend our own `a/<rel>` / `b/<rel>` headers. Diffy
/// always emits exactly those two lines first, so a simple split is
/// safe and avoids pulling in a unified-diff parser just for this.
fn strip_default_headers(s: &str) -> &str {
    let mut iter = s.splitn(3, '\n');
    let _ = iter.next();
    let _ = iter.next();
    iter.next().unwrap_or("")
}
