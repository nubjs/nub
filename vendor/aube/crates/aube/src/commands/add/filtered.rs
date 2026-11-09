use super::build_flags::{
    apply_allow_build_flags, apply_deny_build_flags, reject_conflicting_build_flags,
};
use super::manifest::{AddManifestOptions, update_manifest_for_add};
use super::{AddArgs, install, rollback, supply_chain};
use miette::miette;

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
    if matched.is_empty() {
        return crate::commands::no_projects_matched(
            &root,
            filter,
            crate::commands::NoMatchReport::Quiet,
        );
    }
    let lock = crate::commands::take_project_lock(&root)?;

    let mut snapshot = rollback::Snapshot::default();
    snapshot.capture_install_root(&root)?;
    for pkg in &matched {
        snapshot.capture_manifest(&pkg.dir)?;
    }

    let result: miette::Result<()> = async {
        // CLI build review flags write against the workspace root (where
        // `allowBuilds` lives) — same as the non-filtered path.
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
        supply_chain::run_cli_name_gates(
            &root,
            &args.packages,
            crate::commands::add_supply_chain::ReputationOverrides {
                similar_name: args.allow_similar_name,
                new_name: args.allow_new_name,
                low_downloads: args.allow_low_downloads,
            },
            crate::commands::add_supply_chain::LowDownloadPrompt::Terminal,
            &[],
        )
        .await?;

        for pkg in &matched {
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
        super::apply_dangerously_allow_all_builds(
            &mut install_opts,
            args.dangerously_allow_all_builds,
        );
        install_opts.ignore_scripts = args.ignore_scripts;
        install_opts.workspace_filter = filter.clone();
        install_opts.run_dev_preinstall = true;
        install_opts.script_command = "add";
        let edited: Vec<_> = matched.iter().map(|pkg| pkg.dir.clone()).collect();
        super::apply_add_lifecycle(&mut install_opts, &edited)?;
        // See the sibling `aube add` codepath for why this flag is set:
        // live OSV API on the resolved transitives.
        install_opts.osv_transitive_check = true;
        // `--lockfile-only`: write the lockfile + manifests, skip linking.
        install_opts.lockfile_only = args.lockfile_only;
        // Same as the sibling path: the chained install owns the hooks.
        install_opts.pnpmfile = args.pnpmfile.clone();
        install_opts.global_pnpmfile = args.global_pnpmfile.clone();
        install_opts.ignore_pnpmfile = args.ignore_pnpmfile;
        install::run_with_project_lock(install_opts, &lock).await?;
        Ok(())
    }
    .await;

    let restore_errors = if args.no_save || result.is_err() {
        let restored = snapshot.restore();
        if result.is_err() {
            rollback::warn_restore_failures(&restored.errors);
            if restored.changed && restored.errors.is_empty() {
                eprintln!("Install failed; restored package.json files and lockfile");
            }
        } else if restored.errors.is_empty() {
            eprintln!(
                "Restored {} and lockfile (--no-save)",
                pluralizer::pluralize("package.json file", matched.len() as isize, true)
            );
        }
        restored.errors
    } else {
        Vec::new()
    };

    result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}
