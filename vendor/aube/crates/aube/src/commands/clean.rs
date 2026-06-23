//! `aube clean` / `aube purge` — remove `node_modules/` across the
//! workspace, optionally wiping lockfiles too.
//!
//! Semantics (matches pnpm):
//! - If the root `package.json` defines a `clean` (or `purge`, when
//!   invoked as `aube purge`) script, delegate to `aube run <name>` and
//!   do nothing else. User scripts always win.
//! - Otherwise, walk the workspace (root + every package matched by
//!   `pnpm-workspace.yaml`) and delete each project's `node_modules/`.
//! - With `--lockfile` / `-l`, also remove the root lockfiles:
//!   `aube-lock.yaml`, `pnpm-lock.yaml`, `package-lock.json`,
//!   `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock`. Workspace
//!   children don't carry their own lockfile so the flag only touches
//!   the root.
//!
//! Unlike `aube ci`, `clean` never reinstalls — it's a pure "wipe the
//! tree" command.

use clap::Args;

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Also remove lockfiles at the workspace root.
    ///
    /// Targets `aube-lock.yaml`, `pnpm-lock.yaml`, `package-lock.json`,
    /// `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock`.
    #[arg(short = 'l', long)]
    pub lockfile: bool,
}

/// Lockfile basenames removed by `--lockfile`. Kept in one place so
/// `clean` and any future `purge`-adjacent command see the same set.
/// Lockfile names `aube clean --lockfile` removes: this tool's canonical
/// lockfile plus every foreign format it round-trips. Standalone aube leads
/// with `aube-lock.yaml`.
fn lockfile_names() -> [&'static str; 6] {
    [
        aube_util::embedder().lockfile_basename,
        "pnpm-lock.yaml",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "bun.lock",
    ]
}

pub async fn run(args: CleanArgs) -> miette::Result<Option<i32>> {
    run_as("clean", args).await
}

pub async fn run_purge(args: CleanArgs) -> miette::Result<Option<i32>> {
    run_as("purge", args).await
}

async fn run_as(invoked_as: &str, args: CleanArgs) -> miette::Result<Option<i32>> {
    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;

    // pnpm lets the user override `clean` / `purge` with a package.json
    // script of the same name. If one exists at the root, delegate and
    // stop — the user's script is authoritative.
    let root_pkg = cwd.join("package.json");
    if root_pkg.is_file() {
        let manifest = super::load_manifest(&root_pkg)?;
        if manifest.scripts.contains_key(invoked_as) {
            // `--lockfile` is an aube-specific flag that the user's
            // script almost certainly doesn't know about, so warn
            // loudly that we're handing control over without acting
            // on the flag ourselves.
            if args.lockfile {
                eprintln!(
                    "warning: --lockfile ignored because a `{invoked_as}` script in package.json takes precedence"
                );
            }
            return super::run::run_script(
                invoked_as,
                &[],
                true,
                false,
                &aube_workspace::selector::EffectiveFilter::default(),
            )
            .await;
        }
    }

    // Collect every project dir: workspace root plus any packages
    // matched by `pnpm-workspace.yaml`. `find_workspace_packages` already
    // returns absolute paths and silently returns an empty vec for
    // non-workspace projects, so this works for single-package repos too.
    // A `HashSet` keeps dedup O(n) for large workspaces; we still push
    // to a `Vec` so we preserve the root-first, then discovery-order
    // walk the output summary relies on.
    let mut seen: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::from([cwd.clone()]);
    let mut projects: Vec<std::path::PathBuf> = vec![cwd.clone()];
    match aube_workspace::find_workspace_packages(&cwd) {
        Ok(ws) => {
            for p in ws {
                if seen.insert(p.clone()) {
                    projects.push(p);
                }
            }
        }
        Err(e) => {
            tracing::debug!("skipping workspace discovery: {e}");
        }
    }

    let mut removed_nm = 0usize;
    let mut removed_locks = 0usize;

    // Resolve once against the workspace root — `clean` sweeps every
    // workspace package but they all share the same `modulesDir`
    // (read from `.npmrc` / `aube-workspace.yaml`), so we don't need
    // a per-project lookup.
    let modules_dir_name = super::resolve_modules_dir_name_for_cwd(&cwd);
    for proj in &projects {
        let nm = proj.join(&modules_dir_name);
        // `symlink_metadata` so a `node_modules -> somewhere` symlink
        // gets removed as a link (not followed into its target).
        if nm.symlink_metadata().is_ok() {
            eprintln!("Removing {}", nm.display());
            super::remove_existing(&nm)?;
            removed_nm += 1;
        }
    }

    if args.lockfile {
        for name in lockfile_names() {
            let p = cwd.join(name);
            if p.symlink_metadata().is_ok() {
                eprintln!("Removing {}", p.display());
                super::remove_existing(&p)?;
                removed_locks += 1;
            }
        }
    }

    if removed_nm == 0 && removed_locks == 0 {
        eprintln!("Nothing to clean");
    } else {
        let nm_word = pluralizer::pluralize("directory", removed_nm as isize, false);
        // Only mention lockfiles when we actually removed at least one
        // — "Removed 1 node_modules directory, 0 lockfiles" was just
        // noise when `--lockfile` was passed against a tree that had
        // no lockfile to begin with.
        if removed_locks > 0 {
            eprintln!(
                "Removed {removed_nm} node_modules {nm_word}, {}",
                pluralizer::pluralize("lockfile", removed_locks as isize, true)
            );
        } else {
            eprintln!("Removed {removed_nm} node_modules {nm_word}");
        }
    }

    Ok(None)
}
