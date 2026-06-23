use super::build_flags::{
    apply_allow_build_flags, apply_deny_build_flags, reject_conflicting_build_flags,
};
use super::manifest::{AddManifestOptions, update_manifest_for_add};
use super::{AddArgs, install, no_save, supply_chain};
use miette::{Context, IntoDiagnostic, miette};

pub(super) async fn run(
    args: AddArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if args.packages.is_empty() {
        return Err(miette!("no packages specified"));
    }
    reject_conflicting_build_flags(&args.allow_build, &args.deny_build)?;
    let cwd = crate::dirs::cwd()?;
    // The workspace root — not the child `cwd` — is what owns the
    // lockfile and the project lock in yarn / npm / bun monorepos.
    // Taking the lock or snapshotting the lockfile against `cwd` would
    // target a stale subpackage path, letting `install::run` (which
    // walks up) mutate the real root lockfile and then silently skip
    // the restore under `--no-save`.
    let (root, matched) = crate::commands::select_workspace_packages(&cwd, filter, "add")?;
    let _lock = crate::commands::take_project_lock(&root)?;

    // CLI build review flags write against the workspace root (where
    // `allowBuilds` lives) — same as the non-filtered path. Run before
    // any per-package manifest mutation so a failure can't leave the
    // child manifests half-mutated.
    if !args.allow_build.is_empty() {
        apply_allow_build_flags(&root, &args.allow_build)?;
    }
    if !args.deny_build.is_empty() {
        apply_deny_build_flags(&root, &args.deny_build)?;
    }

    // OSV / downloads gates fire once against the workspace root
    // — every filter-matched importer shares the same
    // `args.packages` list. The Bun-style `securityScanner` is
    // NOT called here: it runs post-resolve from `install::run`
    // against the full resolved graph.
    supply_chain::run_cli_name_gates(&root, &args.packages, args.allow_low_downloads).await?;

    let mut snapshots = Vec::new();
    let lockfile_path = no_save::lockfile_path_for_project(&root)?;
    let root_lockfile_snapshot = if args.no_save {
        no_save::snapshot_lockfile(&lockfile_path)?
    } else {
        None
    };

    let result: miette::Result<()> = async {
        for pkg in &matched {
            let manifest_path = pkg.dir.join("package.json");
            if args.no_save {
                let manifest_bytes = std::fs::read(&manifest_path)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot package.json for --no-save")?;
                snapshots.push((manifest_path.clone(), manifest_bytes));
            }
            update_manifest_for_add(
                &pkg.dir,
                &args.packages,
                AddManifestOptions::from_args(&args),
                !args.no_save,
            )
            .await?;
        }

        let mut install_opts = install::InstallOptions::with_mode(
            crate::commands::chained_frozen_mode(install::FrozenMode::Fix),
        );
        install_opts.workspace_filter = filter.clone();
        // See the sibling `aube add` codepath for why this flag is set:
        // live OSV API on the resolved transitives.
        install_opts.osv_transitive_check = true;
        install::run(install_opts).await?;
        Ok(())
    }
    .await;

    let restore_errors = if args.no_save {
        let mut errors: Vec<miette::Report> = Vec::new();
        let restored = snapshots.len();
        for (manifest_path, manifest_bytes) in snapshots {
            if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &manifest_bytes) {
                errors.push(
                    Result::<(), _>::Err(e)
                        .into_diagnostic()
                        .wrap_err_with(|| {
                            format!(
                                "failed to restore original package.json after --no-save at {}",
                                manifest_path.display()
                            )
                        })
                        .unwrap_err(),
                );
            }
        }
        if let Err(e) = no_save::restore_lockfile(&lockfile_path, &root_lockfile_snapshot) {
            errors.push(e);
        }
        if errors.is_empty() {
            eprintln!(
                "Restored {} and lockfile (--no-save)",
                pluralizer::pluralize("package.json file", restored as isize, true)
            );
        }
        errors
    } else {
        Vec::new()
    };

    result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}
