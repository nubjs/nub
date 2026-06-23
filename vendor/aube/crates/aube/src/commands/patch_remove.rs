//! `aube patch-remove [pkg...]` — drop one or more patch entries from
//! `pnpm.patchedDependencies`, delete the patch file on disk, and
//! re-run install so the linked tree reverts to the unpatched files.
//!
//! Mirrors `pnpm patch-remove`. With no arguments removes every
//! declared patch — useful as a one-shot reset when migrating away
//! from a vendored fix.

use crate::patches::{read_patched_dependencies, remove_patched_dependency};
use clap::Args;
use miette::{IntoDiagnostic, Result, miette};

#[derive(Debug, Args)]
pub struct PatchRemoveArgs {
    /// Patch keys to remove, formatted as `<name>@<version>`.
    ///
    /// Same shape used as the `pnpm.patchedDependencies` map key. With
    /// no arguments, every declared patch is removed.
    pub packages: Vec<String>,
}

pub async fn run(args: PatchRemoveArgs) -> Result<()> {
    let cwd = crate::dirs::project_root()?;
    let declared = read_patched_dependencies(&cwd)?;
    if declared.is_empty() {
        return Err(miette!("no patches declared"));
    }

    let to_remove: Vec<String> = if args.packages.is_empty() {
        declared.keys().cloned().collect()
    } else {
        for key in &args.packages {
            if !declared.contains_key(key) {
                return Err(miette!("no patch declared for {key}"));
            }
        }
        args.packages.clone()
    };

    for key in &to_remove {
        let rel = declared.get(key).cloned().unwrap_or_default();
        let abs = cwd.join(&rel);
        if abs.exists() {
            std::fs::remove_file(&abs)
                .into_diagnostic()
                .map_err(|e| miette!("failed to remove {}: {e}", abs.display()))?;
            eprintln!("Removed {}", abs.display());
        }
        for rewritten in remove_patched_dependency(&cwd, key)? {
            let label = rewritten
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| rewritten.display().to_string());
            eprintln!("Removed {key} from {label}");
        }
    }

    let opts = crate::commands::install::InstallOptions::with_mode(
        crate::commands::install::FrozenMode::Prefer,
    );
    crate::commands::install::run(opts).await?;
    Ok(())
}
