//! Carrying another package manager's lockfile across, once.
//!
//! nub reads no npm, yarn or bun lockfile during an install — a project
//! holding one installs from its own lockfile and resolves afresh, the way
//! pnpm does. `nub pm migrate` is the one command that reads the foreign
//! file: it takes the versions that lockfile pinned, resolves the project
//! with those as preferences, and writes nub's own lockfile in their place.
//!
//! For yarn's and npm's lockfiles this is not a transcode: the versions are
//! preferences, so one the source pinned wins a tie among the versions its
//! range allows and one no longer published is skipped rather than fatal,
//! and the result is a lockfile this engine could have written on its own.
//! Bun's is the exception, because no engine reads it — [`migrate_lockfile`]
//! says where each format is read.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::use_align::{self, NUB_LEGACY_LOCKFILE, NUB_LOCKFILE};

/// The lockfiles another package manager wrote, in the order a migration
/// would prefer them. `bun.lockb` is here so the refusal can name it: it is
/// binary, and nothing in this tree reads it.
const FOREIGN_LOCKFILES: &[&str] = &[
    "yarn.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "bun.lock",
    "bun.lockb",
];

/// The package manager that writes `name`, one of [`FOREIGN_LOCKFILES`].
fn foreign_family(name: &str) -> &'static str {
    match name {
        "yarn.lock" => "yarn",
        "bun.lock" | "bun.lockb" => "bun",
        _ => "npm",
    }
}

/// The lockfiles that answer the question a migration would, so finding one
/// means there is nothing pending. pnpm's is among them because it is nub's
/// own bytes under pnpm's name.
const SETTLED_BY: &[&str] = &[NUB_LOCKFILE, NUB_LEGACY_LOCKFILE, "pnpm-lock.yaml"];

/// The foreign lockfile at `root`, when the project has one and no lockfile
/// of nub's own — the state a migration exists for, and the state the hint
/// on an ordinary install fires in.
pub(crate) fn pending_migration(root: &Path) -> Option<PathBuf> {
    if SETTLED_BY.iter().any(|name| root.join(name).is_file()) {
        return None;
    }
    FOREIGN_LOCKFILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
}

/// The one line an install prints when the project still carries another
/// package manager's lockfile. It is not a warning: installing here is
/// correct and complete, and the only thing lost is the versions that
/// lockfile pinned — which is what the migration carries across.
///
/// The engine's entry point is what prints it, so a build without the engine
/// has nobody to say it.
pub(crate) fn migration_hint(foreign: &Path) -> String {
    format!(
        "nub: {} is another package manager's lockfile and was not read — \
         run `nub pm migrate` to carry the versions it pins into nub's own.",
        foreign.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// `nub pm migrate` — read the project's foreign lockfile once, write nub's
/// own in its format, and remove the source.
///
/// Removing it is the point of the verb: a project left holding both has two
/// answers to what it resolves to, and the next `nub install` would keep
/// printing the hint. `nub import` is the same conversion without the
/// removal, because that is what pnpm's own verb does.
pub(crate) fn run_pm_migrate(cwd: &Path) -> Result<i32> {
    let root = nub_core::workspace::detect::detect_project(cwd)
        .map(|project| project.workspace_root.unwrap_or(project.root))
        .with_context(|| {
            format!(
                "no package.json found from {} — a migration needs a project to migrate",
                cwd.display()
            )
        })?;
    let Some(from) = pending_migration(&root) else {
        match SETTLED_BY.iter().find(|name| root.join(name).is_file()) {
            Some(name) => bail!(
                "this project already has a {name} — there is nothing to migrate. \
                 Delete the other package manager's lockfile if one is still around."
            ),
            None => bail!(
                "no lockfile to migrate — `nub pm migrate` reads a yarn.lock, \
                 package-lock.json, npm-shrinkwrap.json or bun.lock. Run `nub install` \
                 to resolve the project from its package.json instead."
            ),
        }
    };
    // Lockfiles from two package managers are two answers to what the project
    // resolves to. `nub pm use` refuses to guess between them, and so does this:
    // migrating one would remove it and leave the other firing the hint.
    let present: Vec<&str> = FOREIGN_LOCKFILES
        .iter()
        .copied()
        .filter(|name| root.join(name).is_file())
        .collect();
    let mut families: Vec<&str> = present.iter().map(|name| foreign_family(name)).collect();
    families.sort_unstable();
    families.dedup();
    if families.len() > 1 {
        bail!(
            "multiple lockfiles found ({}) — nub can't infer which lockfile to migrate. \
             Remove the stale ones first, then rerun `nub pm migrate`.",
            present.join(", ")
        );
    }
    // A pnpm-incumbent project keeps pnpm's lockfile name; everything else
    // gets nub's. Without the engine there is no pnpm identity to detect and
    // nub's own is the only lockfile this build writes.
    let target = match super::project_identity::detect(&root) {
        super::project_identity::ProjectIdentity::Pnpm => "pnpm",
        super::project_identity::ProjectIdentity::Nub => "nub",
    };
    let written = migrate_lockfile(&root, &from, target)?;
    std::fs::remove_file(&from).with_context(|| format!("removing {}", from.display()))?;
    println!(
        "  {}: written (migrated from {})",
        written.file_name().unwrap_or_default().to_string_lossy(),
        from.file_name().unwrap_or_default().to_string_lossy()
    );
    println!(
        "  {}: removed (migrated)",
        from.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(0)
}

/// Write the target's lockfile from the versions `from` pins. Returns the
/// path written; the caller removes the source only once it has.
///
/// The engine reads yarn's, npm's and bun's lockfiles, so every text
/// lockfile takes its own `import`, which re-resolves with the source's
/// versions as preferences rather than transcribing its graph. Transcoding
/// is what a build without the engine falls back to.
///
/// The brand preflight must already be registered, as it must for any engine
/// call that reads project state.
pub(crate) fn migrate_lockfile(root: &Path, from: &Path, target: &str) -> Result<PathBuf> {
    let name = from.file_name().unwrap_or_default();
    if name == "bun.lockb" {
        bail!(
            "bun.lockb is bun's binary lockfile and nub does not read it — run \
             `bun install --save-text-lockfile` to write a bun.lock first, then \
             rerun the migration."
        );
    }
    engine_import(root, target)
}

/// Migrate through the engine's own `import`, which is where the foreign
/// readers live: it resolves the project with the source lockfile's versions
/// as preferences and writes whichever lockfile the profile names, so a nub
/// project gets `nub.lock` and a pnpm one `pnpm-lock.yaml` with nothing here
/// deciding the filename.
///
/// `--dir` is how the command line says which project, because a migration
/// runs at the workspace root and the process may sit in a member.
fn engine_import(root: &Path, target: &str) -> Result<PathBuf> {
    let argv = vec![
        std::ffi::OsString::from("nub"),
        std::ffi::OsString::from("--dir"),
        std::ffi::OsString::from(root),
        std::ffi::OsString::from("import"),
    ];
    let code = super::pnpm_engine::run(argv)?;
    if code != 0 {
        bail!("the migration did not finish; the project is unchanged");
    }
    let written = root.join(use_align::lockfile_name(target));
    if !written.is_file() {
        bail!(
            "the migration reported success but wrote no {}",
            use_align::lockfile_name(target)
        );
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
