//! `aube deploy` — copy a workspace package into a standalone target
//! directory and install its dependencies there.
//!
//! Mirrors `pnpm --filter=<name> deploy <target>`: we pick one workspace
//! package by name, copy the files it would publish (same selection as
//! `aube pack`), bundle any `workspace:` / `file:` / `link:` deps the
//! deployed package reaches into a staging dir under the target, rewrite
//! the deployed `package.json` to point at the bundled copies, then run a
//! fresh `aube install` rooted at the target dir so the result is a
//! self-contained project — siblings included, no registry round-trip.
//!
//! Implements the common monorepo-CI path:
//!
//!   * required `-F/--filter` (one or more pnpm-style selectors, shared
//!     with the global `-F` flag — exact names, `@scope/*` globs, path
//!     selectors, including dependency-graph selectors)
//!   * `--prod` (default), `--dev`, `--no-prod` (deploy every dep kind),
//!     `--no-optional` forwarded to install and to the manifest rewrite
//!   * single-match fanout drops straight into `<target>`
//!   * multi-match fanout stages each match into
//!     `<target>/<source-dir-basename>/` and requires `<target>` itself
//!     to be empty/missing
//!
//! Workspace siblings + `file:`/`link:` dep targets reachable from the
//! deployed package land at `<target>/.aube-deploy-injected/<id>/`. The
//! deployed manifest (and any nested bundled manifest) gets its
//! `workspace:` / `file:` / `link:` specs rewritten to relative `file:`
//! pointers at those staged copies, so install resolves them as plain
//! local-directory deps. Recursion handles siblings whose own deps are
//! workspace siblings.
//!
//! When the source workspace has a lockfile and no bundling was needed,
//! deploy prunes that lockfile to the deployed package's transitive
//! closure and drops the subset into the target before install runs — a
//! `FrozenMode::Prefer` install then reproduces the workspace's exact
//! resolved versions without re-fetching packuments. When bundling
//! happened, when there is no source lockfile, or the deployed package
//! has workspace-sibling / `link:` / `file:` roots whose rewritten form
//! diverges from the source lockfile, subsetting is skipped and a fresh
//! install runs.
//!
//! Deferred: `--legacy`.
//!
//! The implementation is split across four sibling modules:
//!
//!   * [`staging`] — per-match copy plan + the orchestration around
//!     bundling and rewriting,
//!   * [`injection`] — planning + materializing the bundled
//!     `.aube-deploy-injected/` tree,
//!   * [`filtering`] — `StripFields` / `DepAxis` / dep-selection +
//!     keep-predicate derivations from the CLI flag set,
//!   * [`rewrite`] — `workspace:` / `file:` / `link:` / `catalog:`
//!     specifier rewrites + back-ref handling.
//!
//! `mod.rs` keeps the top-level `run` orchestrator, the `DeployArgs`
//! surface, lockfile subsetting (which is a one-off side-effect tightly
//! coupled to the orchestrator), and the shared `canonicalize` helper.

mod filtering;
mod injection;
mod rewrite;
mod staging;

use crate::commands::install::{self, FrozenMode, InstallOptions};
use aube_manifest::PackageJson;
use clap::Args;
use filtering::{dep_selection_for_args, keep_dep_for_args};
use miette::{Context, IntoDiagnostic, miette};
use staging::{StagedDeploy, stage_one};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Target directory to deploy into.
    ///
    /// Must be empty or not yet exist.
    pub target: PathBuf,
    /// Install only `devDependencies`.
    ///
    /// Implemented by stripping `dependencies` and
    /// `optionalDependencies` from the deployed `package.json` before
    /// install runs.
    #[arg(short = 'D', long, conflicts_with_all = ["prod", "no_prod"])]
    pub dev: bool,
    /// Skip `optionalDependencies`
    #[arg(long)]
    pub no_optional: bool,
    /// Install only production dependencies (default).
    ///
    /// Accepted for pnpm compatibility.
    // Intentionally unread by the deploy code: production is the deploy
    // default, so the `!args.dev && !args.no_prod` axis already captures
    // it. Reach for that, not `args.prod`, when extending the filter.
    #[arg(short = 'P', long, visible_alias = "production")]
    pub prod: bool,
    /// Deploy every dependency kind (production + dev + optional).
    ///
    /// Opts out of the implicit `--prod` deploy default. Useful when a
    /// deployed package needs its devDependencies at runtime (test
    /// harnesses, build-step deploys). Combine with `--no-optional` to
    /// drop optionals while keeping prod + dev. Mutually exclusive with
    /// `--prod` and `--dev`.
    #[arg(long, conflicts_with_all = ["prod", "dev"])]
    pub no_prod: bool,
    /// Fail if any metadata or tarball isn't already in the local cache.
    ///
    /// Never hits the network. Useful in multi-stage Dockerfiles where
    /// an earlier `aube install` already populated the store: deploy
    /// then reproduces a prod-only tree without re-fetching anything.
    #[arg(long, conflicts_with = "prefer_offline")]
    pub offline: bool,
    /// Prefer cached metadata over revalidation; only hit the network on a miss.
    #[arg(long, conflicts_with = "offline")]
    pub prefer_offline: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: DeployArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    if filter.is_empty() {
        return Err(miette!(
            "{}: --filter/-F is required to pick a workspace package",
            aube_util::cmd("deploy")
        ));
    }
    let source_root = crate::dirs::cwd().wrap_err("failed to read current directory")?;

    // Resolve `deployAllFiles` from the source workspace root, before
    // we chdir into any per-match target. `.npmrc` and
    // `pnpm-workspace.yaml` in the source tree are the source of
    // truth — the freshly-created target has neither yet.
    //
    // Use `load_raw` rather than `load_both`: settings resolution only
    // needs the raw YAML map, and `load_both` fails the whole call
    // (including the raw map) when any unrelated typed field
    // mismatches (e.g. `shamefullyHoist: "maybe"`). That would
    // silently drop `deployAllFiles: true`.
    let files = crate::commands::FileSources::load(&source_root);
    let raw_workspace = aube_manifest::workspace::load_raw(&source_root).unwrap_or_default();
    let env = aube_settings::values::process_env();
    let settings_ctx = files.ctx(&raw_workspace, env, &[]);
    let deploy_all_files = aube_settings::resolved::deploy_all_files(&settings_ctx);

    // Discover catalog entries from the source workspace before any
    // chdir. The deploy target has no workspace yaml, so any `catalog:`
    // spec left in the deployed manifest would hit
    // `ERR_AUBE_UNKNOWN_CATALOG` during install — we resolve them up
    // front and rewrite to the concrete range, making the artifact
    // self-contained (same shape as pnpm deploy).
    let catalogs = super::discover_catalogs(&source_root)?;

    let workspace_pkgs = aube_workspace::find_workspace_packages(&source_root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "{}: no workspace packages found. \
             `deploy` requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at {}",
            aube_util::cmd("deploy"),
            source_root.display()
        ));
    }

    // Build (name -> (path, version)) for every workspace package.
    let mut ws_index: BTreeMap<String, (PathBuf, Option<String>)> = BTreeMap::new();
    for dir in &workspace_pkgs {
        let Ok(m) = PackageJson::from_path(&dir.join("package.json")) else {
            continue;
        };
        if let Some(n) = m.name {
            ws_index.insert(n, (dir.clone(), m.version));
        }
    }

    let selected =
        aube_workspace::selector::select_workspace_packages(&source_root, &workspace_pkgs, &filter)
            .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    let mut matches: Vec<(String, PathBuf)> = selected
        .into_iter()
        .filter_map(|pkg| pkg.name.map(|name| (name, pkg.dir)))
        .collect();
    matches.sort_by(|a, b| a.0.cmp(&b.0));

    if matches.is_empty() {
        let names: Vec<&str> = ws_index.keys().map(String::as_str).collect();
        return Err(miette!(
            "{}: --filter {:?} did not match any workspace package. Known: {}",
            aube_util::cmd("deploy"),
            filter,
            names.join(", ")
        ));
    }

    // Resolve target root (relative to the source root — the in-process
    // single-match path chdir's into the target before install runs, so
    // any relative path resolved after that would be wrong).
    let target_root = if args.target.is_absolute() {
        args.target.clone()
    } else {
        source_root.join(&args.target)
    };

    // Work out the real target directory per match. Single match keeps
    // the pre-fanout layout: drop straight into `target_root`. Multi-
    // match requires `target_root` itself to be empty/missing and
    // writes one subdir per package named after the source workspace
    // folder (e.g. `packages/lib` → `<target>/lib`). Using the source
    // basename rather than the package name keeps scoped names
    // (`@test/lib`) out of the deploy path so we don't have to URL-
    // encode or collapse slashes.
    let plan: Vec<(String, PathBuf, PathBuf)> = if matches.len() == 1 {
        let (name, src) = matches.into_iter().next().unwrap();
        vec![(name, src, target_root.clone())]
    } else {
        staging::ensure_target_writable(&target_root)?;
        let mut used: BTreeMap<String, String> = BTreeMap::new();
        let mut v = Vec::with_capacity(matches.len());
        for (name, src) in matches {
            let base = src
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    miette!(
                        "{}: workspace package {} has no directory name",
                        aube_util::cmd("deploy"),
                        src.display()
                    )
                })?;
            if let Some(prev) = used.insert(base.clone(), name.clone()) {
                return Err(miette!(
                    "{}: workspace packages {prev:?} and {name:?} both live in a directory named {base:?}; \
                     multi-package deploy uses the source basename as the target subdir, so these would collide",
                    aube_util::cmd("deploy")
                ));
            }
            v.push((name, src, target_root.join(&base)));
        }
        v
    };

    // Stage every target (copy + manifest rewrite) up front. Running
    // staging for all matches before any install means a multi-package
    // fanout can't half-install one package and then fail on a copy
    // error in the next.
    let mut staged: Vec<StagedDeploy> = Vec::with_capacity(plan.len());
    for (_name, source_pkg_dir, target) in &plan {
        staged.push(stage_one(
            source_pkg_dir,
            target,
            &ws_index,
            &catalogs,
            &args,
            deploy_all_files,
        )?);
    }

    for (s, source_pkg_dir) in staged.iter().zip(plan.iter().map(|(_, src, _)| src)) {
        // Seed the target with a pruned copy of the source workspace
        // lockfile before chdir'ing into the target. Both the source
        // read and the target write use absolute paths, so ordering
        // with `retarget_cwd` doesn't matter for correctness — doing
        // it before keeps the side-effect timeline "stage → seed →
        // install" readable top-to-bottom. Returns `false` when we
        // fell back to a fresh install (no source lockfile, the
        // importer had local roots we couldn't seed, or staging
        // bundled local refs in a way that diverges from the source
        // lockfile).
        let seeded = if s.bundled_local_refs {
            tracing::debug!(
                "deploy: bundled local refs into {}; skipping lockfile subset",
                s.target.display()
            );
            false
        } else {
            seed_target_lockfile(&source_root, source_pkg_dir, &s.target, &args)?
        };

        super::retarget_cwd(&s.target)?;

        // `no_optional` here is only the user flag — don't fold `--dev` in.
        // The `StripFields` in `stage_one` already dropped top-level
        // `optionalDependencies` from the manifest for `--dev`, which is
        // what pnpm does. Setting `InstallOptions.no_optional` on top of
        // that would also filter out *transitive* optional deps of
        // devDependencies (e.g. an optional sub-dep of `jest`), breaking
        // dev tooling at runtime.
        //
        // `mode`: when we seeded a subset lockfile, `Prefer` lets the
        // install reproduce the source workspace's pinned versions
        // without re-resolving against the registry. When we didn't,
        // fall back to `No` so install resolves from scratch — same as
        // the pre-subsetting behavior.
        let mode = if seeded {
            FrozenMode::Prefer
        } else {
            FrozenMode::No
        };
        let network_mode = if args.offline {
            aube_registry::NetworkMode::Offline
        } else if args.prefer_offline {
            aube_registry::NetworkMode::PreferOffline
        } else {
            aube_registry::NetworkMode::Online
        };
        let opts = InstallOptions {
            control: install::InstallControl::default(),
            embedder_node_bin_dir: None,
            project_dir: Some(s.target.clone()),
            mode,
            dep_selection: dep_selection_for_args(&args),
            ignore_pnpmfile: false,
            pnpmfile: None,
            global_pnpmfile: None,
            ignore_scripts: false,
            dry_run: false,
            lockfile_only: false,
            merge_git_branch_lockfiles: false,
            dangerously_allow_all_builds: false,
            network_mode,
            minimum_release_age_override: None,
            strict_no_lockfile: false,
            force: false,
            cli_flags: vec![(
                "dangerously-allow-all-builds".to_string(),
                "false".to_string(),
            )],
            env_snapshot: aube_settings::values::capture_env(),
            git_prepare_depth: 0,
            inherited_build_policy: None,
            build_policy_override: None,
            workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
            skip_root_lifecycle: false,
            // Deploys are lockfile-driven by definition. Don't
            // force the live API; install::run's fresh-resolution
            // detection still kicks in if the resolver picks a
            // version that wasn't pinned.
            osv_transitive_check: false,
        };
        install::run(opts).await?;

        println!(
            "deployed {}@{} to {}",
            s.name,
            s.version,
            s.target.display()
        );
    }

    Ok(())
}

/// Canonicalize a path, falling back to the input on error. Used as a
/// stable identity key for siblings/local refs across rewriters that
/// reach the same target via different relative specs.
pub(super) fn canonicalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Attempt to seed `target` with a subset of the source workspace's
/// lockfile, pruned to the deployed package's transitive closure.
/// Returns `true` iff a lockfile was written; `false` means we fell
/// back to the fresh-install path.
///
/// Fall-back (return `Ok(false)`) happens when:
///   * the source workspace has no lockfile (nothing to subset),
///   * the source lockfile can't be parsed,
///   * the deployed importer isn't in the source lockfile (stale
///     or never-installed workspace),
///   * any retained direct dep is backed by a local source (`link:`,
///     `file:` directory, or `file:` tarball). Workspace siblings and
///     local file deps can't resolve in a standalone target: the
///     sibling isn't published, and the local path would point
///     outside the deploy tree. Writing a subset lockfile that
///     references them would be strictly worse than letting the
///     fresh install surface the same resolution error at the right
///     layer.
///
/// The subset honors `--prod` / `--dev` / `--no-optional` the same
/// way `stage_one` rewrites the target manifest, so the two agree on
/// which dep fields survive — drift detection would otherwise fire.
/// Graph-wide metadata (`overrides`, `catalogs`,
/// `ignoredOptionalDependencies`) is cleared: the source resolver
/// already baked its effects into `packages:`, and keeping them in
/// the header would only trip drift against the target's minimal
/// package.json.
fn seed_target_lockfile(
    source_root: &Path,
    source_pkg_dir: &Path,
    target: &Path,
    args: &DeployArgs,
) -> miette::Result<bool> {
    // Source workspace root manifest is required by
    // `parse_lockfile_with_kind` (yarn.lock in particular needs the
    // manifest to classify direct vs transitive deps). A workspace
    // without a root `package.json` is unusual but not invalid, so
    // fall back rather than erroring.
    let Ok(source_manifest) = PackageJson::from_path(&source_root.join("package.json")) else {
        tracing::debug!("deploy: workspace root package.json unreadable, skipping lockfile subset");
        return Ok(false);
    };
    let (graph, kind) = match aube_lockfile::parse_lockfile_with_kind(source_root, &source_manifest)
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!("deploy: no usable source lockfile ({e}); fresh install instead");
            return Ok(false);
        }
    };

    // Workspace-relative importer path ("." for root, "packages/lib"
    // for a sibling) — same shape pnpm writes into `importers:`
    // keys, which is what `subset_to_importer` indexes by.
    let importer_path = super::workspace_importer_path(source_root, source_pkg_dir)?;

    let Some(mut subset) = graph.subset_to_importer(&importer_path, keep_dep_for_args(args)) else {
        tracing::debug!(
            "deploy: importer {importer_path:?} not in source lockfile; fresh install instead"
        );
        return Ok(false);
    };

    // Any retained direct dep backed by a local source is a dead
    // end for a standalone target. See the function doc for the
    // reasoning — short version: the sibling isn't published and
    // the `link:` / `file:` path points outside the deploy tree.
    let has_local_root = subset.root_deps().iter().any(|d| {
        subset
            .get_package(&d.dep_path)
            .and_then(|p| p.local_source.as_ref())
            .is_some_and(|src| {
                matches!(
                    src,
                    aube_lockfile::LocalSource::Link(_)
                        | aube_lockfile::LocalSource::Portal(_)
                        | aube_lockfile::LocalSource::Exec(_)
                        | aube_lockfile::LocalSource::Directory(_)
                        | aube_lockfile::LocalSource::Tarball(_)
                )
            })
    });
    if has_local_root {
        tracing::debug!("deploy: source importer has link:/file: roots; fresh install instead");
        return Ok(false);
    }

    // Drop workspace-scope metadata the target can't honor. Their
    // effects already live in `packages:` (the resolver baked them
    // in), so keeping them here would only trip drift detection
    // against the target's minimal package.json — which has no
    // `pnpm.overrides`, no `catalog:` refs, no
    // `pnpm.ignoredOptionalDependencies`.
    subset.overrides.clear();
    subset.ignored_optional_dependencies.clear();
    subset.catalogs.clear();

    // Prune `times` to match the subset's `packages`. `times` isn't
    // part of drift detection, so keeping the source workspace's
    // full `time:` map doesn't break `FrozenMode::Prefer`, but it
    // bloats the target lockfile with timestamps for every package
    // the source workspace ever resolved — including the ones we
    // just pruned from the closure.
    //
    // `times` is keyed by the canonical `name@version` (no peer
    // suffix) while `subset.packages` is keyed by the full dep_path
    // (which can carry a `(peer@ver)` suffix), so a direct
    // `contains_key` check against `packages` would silently drop
    // timestamps for any package resolved with a peer context.
    // Build the canonical key set from `LockedPackage.name` /
    // `.version` and filter against that.
    let canonical_keys: std::collections::HashSet<String> =
        subset.packages.values().map(|pkg| pkg.spec_key()).collect();
    subset.times.retain(|key, _| canonical_keys.contains(key));

    // Re-read the rewritten target manifest. The writer uses `name`
    // / `version` / direct-dep specifiers to stamp the lockfile
    // header correctly; using the source workspace root manifest
    // would fill in the wrong name for the deployed package.
    let target_manifest = PackageJson::from_path(&target.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("deploy: failed to re-read rewritten target package.json")?;

    aube_lockfile::write_lockfile_as(target, &subset, &target_manifest, kind)
        .into_diagnostic()
        .wrap_err("deploy: failed to write subset lockfile into target")?;
    Ok(true)
}
