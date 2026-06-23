pub(crate) mod build_flags;
mod filtered;
mod global;
mod manifest;
mod no_save;
mod spec;
mod supply_chain;

use super::install;
use build_flags::{
    apply_allow_build_flags, apply_deny_build_flags, parse_allow_build_value,
    parse_deny_build_value, reject_conflicting_build_flags,
};
use clap::Args;
use manifest::{
    AddManifestOptions, update_manifest_for_add, workspace_protocol_override_from_flags,
};
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Package(s) to add
    pub packages: Vec<String>,
    /// Add as dev dependency
    #[arg(short = 'D', long)]
    pub save_dev: bool,
    /// Pin the exact resolved version (no `^` prefix)
    #[arg(short = 'E', long)]
    pub save_exact: bool,
    /// Install the package globally.
    ///
    /// Installs into the aube/pnpm global directory and links its
    /// binaries into the global bin directory. Mirrors `pnpm add -g`.
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Add as optional dependency
    #[arg(short = 'O', long)]
    pub save_optional: bool,
    /// Pre-approve a dependency's lifecycle scripts as part of the add.
    ///
    /// Writes `allowBuilds: { <pkg>: true }` into the workspace yaml
    /// (or `package.json#aube.allowBuilds`) before the install runs,
    /// so the named package's `preinstall` / `install` / `postinstall`
    /// scripts execute on this invocation. Repeatable — pass the flag
    /// once per package. Mirrors `pnpm add --allow-build=<pkg>`.
    ///
    /// Conflicts with `--no-save`, which only snapshots `package.json`
    /// and the lockfile and would leave an orphaned approval in the
    /// workspace yaml on restore. Also conflicts with `--deny-build` for
    /// the same package name.
    #[arg(
        long = "allow-build",
        value_name = "PKG",
        conflicts_with = "no_save",
        require_equals = true,
        value_parser = parse_allow_build_value,
    )]
    pub allow_build: Vec<String>,
    /// Bypass the [`lowDownloadThreshold`] confirm prompt / refusal for
    /// this invocation.
    ///
    /// `aube add` looks up each candidate's weekly download count and
    /// prompts (interactive) or fails (CI) when the count is below
    /// [`lowDownloadThreshold`]. The flag is intended for the cases
    /// where you've already verified the package out-of-band — adding
    /// a brand-new niche tool, a fresh fork, an internal scratch
    /// package — and don't want the prompt to interrupt scripted
    /// workflows. Does not affect the OSV malicious-package check,
    /// which remains a hard block.
    #[arg(long)]
    pub allow_low_downloads: bool,
    /// Mark a dependency's lifecycle scripts as reviewed and denied.
    ///
    /// Writes `allowBuilds: { <pkg>: false }` into the workspace yaml
    /// (or `package.json#aube.allowBuilds`) before the install runs,
    /// so the named package's lifecycle scripts stay skipped without
    /// tripping `strictDepBuilds=true`. Repeatable — pass the flag
    /// once per package.
    ///
    /// Conflicts with `--no-save`, which only snapshots `package.json`
    /// and the lockfile and would leave an orphaned denial in the
    /// workspace yaml on restore. Also conflicts with `--allow-build` for
    /// the same package name.
    #[arg(
        long = "deny-build",
        value_name = "PKG",
        conflicts_with = "no_save",
        require_equals = true,
        value_parser = parse_deny_build_value,
    )]
    pub deny_build: Vec<String>,
    /// Skip lifecycle scripts (no-op; aube already skips by default).
    #[arg(long, hide = true)]
    pub ignore_scripts: bool,
    /// Install without persisting the dependency to `package.json`.
    ///
    /// Snapshots `package.json` and the lockfile, links the named
    /// packages into `node_modules`, and then restores both files —
    /// so the dependency is usable for the current process but the
    /// project's committed state is untouched.
    ///
    /// Handy for one-off experiments and for scripts that install a
    /// tool transiently. Mirrors `pnpm add --no-save`. Conflicts with
    /// `-g`/`--global`, which has to persist the install to its global
    /// manifest.
    #[arg(long, conflicts_with = "global")]
    pub no_save: bool,
    /// Inverse of `--save-workspace-protocol`.
    ///
    /// Forces the manifest specifier into a registry-style spec
    /// (`^<version>`) for this invocation, even when
    /// `linkWorkspacePackages` matched a local sibling. The install
    /// pipeline still prefers the local workspace copy at resolve
    /// time — this flag only controls what's written to
    /// `package.json`. Mirrors `pnpm add --no-save-workspace-protocol`.
    #[arg(long, overrides_with = "save_workspace_protocol")]
    pub no_save_workspace_protocol: bool,
    /// Save the new dependency into the workspace's default catalog.
    ///
    /// Writes `catalog:` into `package.json` and seeds/upserts the
    /// resolved range under `catalog:` in the workspace yaml. Mirrors
    /// `pnpm add --save-catalog`.
    ///
    /// Workspace and aliased specs (`workspace:*`, `npm:`, `jsr:`) are
    /// never catalogized — the manifest gets the original spec and
    /// the catalog yaml is left alone. If the package is already in
    /// the target catalog, the existing entry is preserved (never
    /// overwritten); the manifest then gets `catalog:` only when the
    /// existing entry is compatible with the user's range.
    ///
    /// Conflicts with `--no-save`: catalog mutations write to the
    /// workspace yaml, which the `--no-save` restore path doesn't
    /// snapshot — combining the two would silently leave an orphaned
    /// catalog entry behind.
    #[arg(long, conflicts_with_all = ["save_catalog_name", "no_save"])]
    pub save_catalog: bool,
    /// Save the new dependency into a *named* catalog.
    ///
    /// Writes the entry to `catalogs.<name>` in the workspace yaml and
    /// `catalog:<name>` into `package.json`. Same workspace/alias
    /// exclusions and `--no-save` conflict as `--save-catalog`. Mirrors
    /// `pnpm add --save-catalog-name=<name>`.
    #[arg(long, value_name = "NAME", conflicts_with = "no_save")]
    pub save_catalog_name: Option<String>,
    /// Add as a peer dependency (written to `peerDependencies` in
    /// package.json).
    ///
    /// By convention you usually pair this with `--save-dev` so the
    /// peer is also installed for local development; that's what pnpm
    /// does.
    #[arg(long, conflicts_with = "save_optional")]
    pub save_peer: bool,
    /// Force the manifest specifier into `workspace:` form for this
    /// invocation, overriding `saveWorkspaceProtocol` from the
    /// workspace yaml / `.npmrc` / env.
    ///
    /// Only meaningful when `linkWorkspacePackages` (or a workspace
    /// sibling already exists for the named package). With this flag
    /// the entry written to `package.json` is `workspace:^` (rolling)
    /// or `workspace:^<version>` (pinned), depending on the resolved
    /// `saveWorkspaceProtocol` value.
    #[arg(long, overrides_with = "no_save_workspace_protocol")]
    pub save_workspace_protocol: bool,
    /// Add the dependency to the workspace root's `package.json`.
    ///
    /// Applies regardless of the current working directory: walks up
    /// from cwd looking for `aube-workspace.yaml`, `pnpm-workspace.yaml`,
    /// or a `package.json` with a `workspaces` field and runs the add
    /// against that directory.
    #[arg(short = 'w', long, conflicts_with = "global")]
    pub workspace: bool,
    /// Allow `add` to run in a workspace root.
    ///
    /// By default aube refuses to add dependencies to the root
    /// `package.json` of a workspace (a directory containing
    /// `aube-workspace.yaml`, `pnpm-workspace.yaml`, or a `package.json`
    /// with a `workspaces` field) because deps added there end up
    /// shared by every package and usually reflect a mistake. Pass
    /// this flag to opt in. Mirrors `pnpm add -W`.
    #[arg(short = 'W', long)]
    pub ignore_workspace_root_check: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: AddArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    if !filter.is_empty() && !args.global && !args.workspace {
        return filtered::run(args, &filter).await;
    }

    let AddArgs {
        packages,
        global,
        save_dev,
        save_optional,
        save_exact,
        save_peer,
        save_workspace_protocol,
        no_save_workspace_protocol,
        workspace,
        ignore_scripts: _,
        no_save,
        ignore_workspace_root_check,
        save_catalog,
        save_catalog_name,
        allow_build,
        deny_build,
        allow_low_downloads,
        lockfile,
        network,
        virtual_store,
    } = args;
    let save_catalog_target = save_catalog_name.or_else(|| {
        if save_catalog {
            Some("default".to_string())
        } else {
            None
        }
    });
    let packages = &packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }
    reject_conflicting_build_flags(&allow_build, &deny_build)?;

    if global {
        return global::run_global(
            packages,
            allow_build,
            deny_build,
            allow_low_downloads,
            lockfile,
            network,
            virtual_store,
        )
        .await;
    }

    // `--workspace` / `-w`: redirect the add at the workspace root
    // (directory containing `aube-workspace.yaml` / `pnpm-workspace.yaml`)
    // before anything reads `dirs::cwd()`. We chdir into it so the
    // downstream install pipeline treats the root as the project.
    if workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    // pnpm `install <pkg>` (= aube `add <pkg>`) creates an empty
    // package.json when run in a directory with no manifest, so users
    // can bootstrap a project with a single command. Match that: if no
    // ancestor has a package.json (within the home boundary), write a
    // minimal `{}` in cwd before resolving the project root. The
    // `--global`/`-g` path returned earlier; `--workspace` already
    // redirected to a known root above.
    let initial_cwd = crate::dirs::cwd()?;
    if crate::dirs::find_project_root(&initial_cwd).is_none() {
        std::fs::write(initial_cwd.join("package.json"), "{}\n")
            .into_diagnostic()
            .wrap_err("failed to create package.json")?;
    }
    let cwd = crate::dirs::project_root()?;

    // Refuse to add into a workspace root unless the caller opts out.
    // Matches pnpm: deps added here are shared by every workspace
    // package and usually reflect a mistake. `-W` /
    // `--ignore-workspace-root-check` bypasses the check, and `-w` /
    // `--workspace` implies the bypass since the user explicitly
    // targeted the root. We trip on a *declared* package-pattern list,
    // not on the materialized glob — an empty `packages/*` directory
    // is still a workspace root the user should opt into. Bare
    // catalog-only yaml is not a workspace root, and a `package.json`
    // without a `workspaces` field isn't either.
    if !ignore_workspace_root_check && !workspace {
        // `WorkspaceConfig::load` already returns an empty `packages`
        // list when no yaml exists, so propagating errors here only
        // surfaces genuine yaml problems (permission denied, malformed
        // YAML) instead of silently letting `add` proceed against what
        // might actually be a workspace root.
        let ws = aube_manifest::WorkspaceConfig::load(&cwd)
            .into_diagnostic()
            .wrap_err("failed to read workspace config")?;
        let yaml_has_packages = !ws.packages.is_empty();
        // `package.json` read errors fall through intentionally: the
        // install pipeline below re-reads and parses the same file and
        // surfaces a richer miette diagnostic pointing at the offending
        // byte. Duplicating that error here would double-report.
        let pkg_json_has_workspaces =
            aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
                .ok()
                .and_then(|m| m.workspaces)
                .is_some_and(|w| !w.patterns().is_empty());
        if yaml_has_packages || pkg_json_has_workspaces {
            return Err(miette!(
                "refusing to add dependencies to the workspace root. \
                 If this is intentional, pass --ignore-workspace-root-check (-W)."
            ));
        }
    }

    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    // 1. Read existing package.json. Snapshot the raw bytes when
    // `--no-save` is in effect so we can restore both the manifest
    // *and* the lockfile after the resolver/install pipeline (both
    // re-read from disk) has done its work — the user gets the new
    // package linked into `node_modules` while their committed
    // project state stays exactly as they wrote it.
    //
    // The lockfile path matches whatever
    // `write_lockfile_preserving_existing` will write to: detect the
    // existing lockfile kind on disk (pnpm, npm, yarn, bun, …) so a
    // project using `pnpm-lock.yaml` doesn't end up with both a
    // restored aube-lock.yaml *and* a leftover modified pnpm-lock.yaml.
    // When no lockfile exists yet the resolver falls back to aube's
    // own format, so we target that path and the restore step deletes
    // it (since `lockfile_bytes` is `None`).
    let lockfile_path = no_save::lockfile_path_for_project(&cwd)?;
    let no_save_snapshot = if no_save {
        Some(no_save::snapshot_manifest_and_lockfile(
            &manifest_path,
            &lockfile_path,
        )?)
    } else {
        None
    };
    // `--allow-build=<pkg>` / `--deny-build=<pkg>` pre-review dep
    // lifecycle scripts as part of the add. The install pipeline
    // re-reads the map from disk, so writing before manifest mutation
    // keeps failure-mode reasoning local.
    if !allow_build.is_empty() {
        apply_allow_build_flags(&cwd, &allow_build)?;
    }
    if !deny_build.is_empty() {
        apply_deny_build_flags(&cwd, &deny_build)?;
    }

    // OSV / downloads gates fire pre-manifest-mutation — they're
    // human-intent signals that key off the typed package names,
    // so a refusal here leaves `package.json` untouched. The
    // Bun-style `securityScanner` is intentionally NOT called
    // here: it runs post-resolve from `install::run` against the
    // full resolved graph (matching Bun's contract), with
    // concrete versions + transitives the OSV/downloads probes
    // wouldn't see at this stage.
    supply_chain::run_cli_name_gates(&cwd, packages, allow_low_downloads).await?;

    update_manifest_for_add(
        &cwd,
        packages,
        AddManifestOptions {
            save_dev,
            save_exact,
            save_optional,
            save_peer,
            save_catalog: save_catalog_target,
            workspace_protocol_override: workspace_protocol_override_from_flags(
                save_workspace_protocol,
                no_save_workspace_protocol,
            ),
        },
        !no_save,
    )
    .await?;

    // 4. Run install. It re-reads the mutated package.json, runs the
    // resolver (reusing locked entries for unchanged specs), writes the
    // lockfile, and links node_modules in one pipeline. `Fix` mode is
    // the right semantic here: package.json just gained a new spec,
    // so the lockfile is by definition stale on that one entry — Prefer
    // would risk taking the from-lockfile fast path and missing the
    // new dep. Wrapping in a `Result` so the restore step below runs
    // even on failure — a network error mid-resolve would otherwise
    // leave the mutated `package.json` on disk, breaking `--no-save`.
    // `with_mode()` already skips root lifecycle hooks (chained-call
    // contract) so `aube add` doesn't re-run the root postinstall /
    // prepare on every invocation.
    // `osv_transitive_check = true` routes the resolved transitive
    // set through OSV's `MAL-*` batch query post-resolve, so a
    // malicious dep-of-dep fails the install with the same
    // `ERR_AUBE_MALICIOUS_PACKAGE` as the CLI-name gate above.
    let mut install_opts =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Fix));
    install_opts.osv_transitive_check = true;
    let pipeline_result: miette::Result<()> = install::run(install_opts).await;

    // 5. Under `--no-save`, restore the snapshotted `package.json` and
    // lockfile so neither shows up in `git status`. The user's
    // `node_modules` keeps the freshly linked package — matching
    // pnpm's `--no-save` semantics. We do this regardless of whether
    // the install succeeded so failures still leave the project
    // pristine. If the lockfile didn't exist before, delete the one
    // we just wrote.
    //
    // Both restores are attempted independently — if the manifest
    // write fails, we still try the lockfile restore so the project
    // doesn't get stuck in a half-mutated state. Any errors from this
    // step (and the captured `pipeline_result`) are folded together
    // before returning, so the caller sees the *first* relevant
    // failure rather than silently dropping later ones.
    let restore_errors = if let Some(snapshot) = no_save_snapshot {
        let errors =
            no_save::restore_manifest_and_lockfile(snapshot, &manifest_path, &lockfile_path);
        if errors.is_empty() {
            eprintln!("Restored package.json and lockfile (--no-save)");
        }
        errors
    } else {
        Vec::new()
    };

    // Order matters: surface the pipeline error first when present —
    // it's the root cause and the restore errors are downstream
    // fallout. With no pipeline error, surface the first restore
    // failure (subsequent ones are usually variants of the same
    // filesystem problem).
    pipeline_result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}
