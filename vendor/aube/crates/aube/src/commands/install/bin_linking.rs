use aube_lockfile::dep_path_filename::dep_path_to_filename;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) type PkgJsonCache = BTreeMap<String, Option<serde_json::Value>>;

/// Per-install cache of workspace-package `package.json` reads. Keyed
/// by the workspace dir on disk so a popular tooling package consumed
/// by many importers gets read and parsed once, not once per consumer.
pub(crate) type WsPkgJsonCache = BTreeMap<PathBuf, Option<serde_json::Value>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedBinEntry {
    File(Vec<u8>),
    Symlink(PathBuf),
    Other,
}

/// Exact shim files created during the pre-lifecycle linking pass, keyed by
/// their `.bin` directory and command name. Snapshots distinguish unchanged
/// Aube output from lifecycle-produced replacements on every platform.
#[derive(Debug, Default)]
pub(crate) struct ManagedBinLinks {
    entries: BTreeMap<PathBuf, BTreeMap<String, BTreeMap<PathBuf, ManagedBinEntry>>>,
    seen: BTreeMap<PathBuf, BTreeSet<String>>,
    capture: bool,
}

impl ManagedBinLinks {
    pub(crate) fn capturing() -> Self {
        Self {
            capture: true,
            ..Default::default()
        }
    }
}
pub(crate) type PreservedBinLinks = BTreeMap<PathBuf, BTreeSet<String>>;

pub(crate) struct LinkDepBinsInput<'a> {
    pub(crate) aube_dir: &'a Path,
    pub(crate) graph: &'a aube_lockfile::LockfileGraph,
    pub(crate) virtual_store_dir_max_length: usize,
    pub(crate) placements: Option<&'a aube_linker::HoistedPlacements>,
    pub(crate) shim_opts: aube_linker::BinShimOptions<'a>,
    pub(crate) cache: &'a mut PkgJsonCache,
    pub(crate) managed: &'a mut ManagedBinLinks,
    pub(crate) preserved: Option<&'a PreservedBinLinks>,
}

pub(super) struct LinkAllBinsInput<'a> {
    pub(super) project_dir: &'a Path,
    pub(super) settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub(super) modules_dir_name: &'a str,
    pub(super) aube_dir: &'a Path,
    pub(super) graph: &'a aube_lockfile::LockfileGraph,
    pub(super) virtual_store_dir_max_length: usize,
    pub(super) placements: Option<&'a aube_linker::HoistedPlacements>,
    pub(super) ws_dirs: &'a BTreeMap<String, PathBuf>,
    pub(super) manifests: &'a [(String, aube_manifest::PackageJson)],
    pub(super) manifest: &'a aube_manifest::PackageJson,
    pub(super) node_linker: aube_linker::NodeLinker,
    pub(super) has_workspace: bool,
    /// Layout-only pass (`--virtual-store-only`): no `.bin` is written at all.
    pub(super) virtual_store_only: bool,
    /// The three inputs to [`super::default_trust::dep_build_scripts_may_run`],
    /// which decides whether the per-dep `.bin` pass runs. Kept as inputs
    /// rather than a pre-folded boolean so the link side and the lifecycle
    /// side cannot drift: the `defaultTrust` floor can authorize a package's
    /// build with no explicit allow rule, and those scripts need their own
    /// deps' CLIs on PATH.
    pub(super) ignore_scripts: bool,
    pub(super) has_any_allow_rule: bool,
    pub(super) floor_may_allow_any: bool,
    pub(super) preserved: Option<&'a PreservedBinLinks>,
}

/// Compute the on-disk directory a dep's materialized package lives
/// in. Matches the path `aube-linker` writes under
/// `node_modules/.aube/<escaped dep_path>/node_modules/<name>`.
///
/// `virtual_store_dir_max_length` must match the value the linker
/// was built with (see `install::run` for the single source of
/// truth) — otherwise long `dep_path`s that trigger the
/// truncate-and-hash fallback inside `dep_path_to_filename` will
/// encode to a different filename than the one the linker wrote,
/// and this function will return a path that doesn't exist.
pub(crate) fn materialized_pkg_dir(
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> std::path::PathBuf {
    // In hoisted mode the package was materialized directly into
    // `node_modules/<...>/<name>/` and its path is recorded in
    // `placements`. Fall back to the isolated `.aube/<dep_path>`
    // convention when either the mode is isolated (`placements` is
    // `None`) or the hoisted planner didn't place this specific
    // dep_path (e.g. filtered by `--prod` / `--no-optional`).
    // `aube_dir` is the resolved `virtualStoreDir` — the install
    // driver threads it in via `commands::resolve_virtual_store_dir`
    // so a custom override lands on the same path the linker wrote
    // to.
    if let Some(placements) = placements
        && let Some(p) = placements.package_dir(dep_path)
    {
        return p.to_path_buf();
    }
    aube_dir
        .join(dep_path_to_filename(dep_path, virtual_store_dir_max_length))
        .join("node_modules")
        .join(name)
}

/// Directory holding the dep's own `node_modules/` — i.e. the dir
/// that contains both `<name>` and its sibling symlinks. For scoped
/// packages (`@scope/name`) `package_dir` is two levels below that
/// `node_modules/`, so we strip the extra `@scope` hop. Used to
/// locate the per-dep `.bin/` for transitive lifecycle-script bins.
pub(crate) fn dep_modules_dir_for(package_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    if name.starts_with('@') {
        package_dir
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| package_dir.to_path_buf())
    } else {
        package_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| package_dir.to_path_buf())
    }
}

/// Read a dep's `package.json` from its materialized directory.
///
/// Earlier revisions of this file went through
/// `package_indices[dep_path]` and read
/// `stored.store_path.join("package.json")` from the CAS. That
/// stopped working once `fetch_packages_with_root` learned to skip
/// `load_index` for packages whose `.aube/<dep_path>` already exists
/// (the `AlreadyLinked` fast path) — the indices map is sparse on
/// warm installs, and every caller that reached for
/// `package_indices.get(..)?.get("package.json")` silently dropped
/// those deps via the `continue` or `?` on the missing key.
///
/// Read the hardlinked file at the materialized location instead:
/// same bytes, zero dependency on the sparse indices map, and
/// doesn't require a cache miss to surface when the virtual store is
/// intact.
///
/// Error policy: `Ok(None)` only when the file is legitimately
/// missing (e.g. a package that ships without a top-level
/// `package.json`, or hasn't been materialized yet). Every other
/// `std::io::Error` — permission denied, short reads, disk errors —
/// bubbles up as `Err` so the user sees a real failure instead of a
/// silently dropped bin link. Parse errors likewise propagate.
fn read_materialized_pkg_json(
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Option<serde_json::Value>> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    let pkg_json_path = pkg_dir.join("package.json");
    let content = match std::fs::read_to_string(&pkg_json_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(miette!(
                "failed to read package.json for {name} at {}: {e}",
                pkg_json_path.display()
            ));
        }
    };
    let value = aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
        .map_err(miette::Report::new)
        .wrap_err_with(|| format!("failed to parse package.json for {name}"))?;
    Ok(Some(value))
}

#[allow(clippy::too_many_arguments)]
fn read_materialized_pkg_json_cached(
    cache: &mut PkgJsonCache,
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Option<serde_json::Value>> {
    if let Some(value) = cache.get(dep_path) {
        return Ok(value.clone());
    }
    let value = read_materialized_pkg_json(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    )?;
    cache.insert(dep_path.to_string(), value.clone());
    Ok(value)
}

/// Create top-level + bundled bin symlinks for one dep. Extracted so
/// both the root-importer pass (`link_bins`) and the per-workspace
/// loop use the same code path.
#[allow(clippy::too_many_arguments)]
pub(super) fn link_bins_for_dep(
    cache: &mut PkgJsonCache,
    aube_dir: &std::path::Path,
    bin_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    let pkg_json = read_materialized_pkg_json_cached(
        cache,
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    )?;
    link_bins_of_pkg_dir(
        bin_dir,
        &pkg_dir,
        name,
        pkg_json.as_ref(),
        graph,
        dep_path,
        shim_opts,
        managed,
        preserved,
    )
}

/// Shim one package's own bins (plus its bundled deps') into `bin_dir`,
/// resolving every target against the caller's `pkg_dir`.
///
/// Split out of [`link_bins_for_dep`] so the hoisted pass can hand in a
/// *concrete* placement directory. A package whose name conflicts with a
/// shallower version is materialized once per site, and each site's shims
/// must point at its own copy — `materialized_pkg_dir` only ever returns
/// the shallowest, so the deeper sites need their path passed in.
#[allow(clippy::too_many_arguments)]
fn link_bins_of_pkg_dir(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    name: &str,
    pkg_json: Option<&serde_json::Value>,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    if let Some(pkg_json) = pkg_json {
        if let Some(bin) = pkg_json.get("bin") {
            link_bin_entries(
                bin_dir,
                pkg_dir,
                Some(name),
                bin,
                shim_opts,
                managed,
                preserved,
            )?;
        } else if let Some(dir_bin) = pkg_json.get("directories").and_then(|d| d.get("bin")) {
            // `bin` wins; `directories.bin` is the fallback only.
            link_dir_bins(bin_dir, pkg_dir, dir_bin, shim_opts, managed, preserved)?;
        }
    }
    link_bundled_bins(
        bin_dir, pkg_dir, graph, dep_path, shim_opts, managed, preserved,
    )?;
    Ok(())
}

/// Hoisted layout: shim every placed package's bins into the `.bin` of
/// the `node_modules/` it was hoisted into.
///
/// The isolated layout gets this for free — each package owns a private
/// `.aube/<dep_path>/node_modules/`, and `link_dep_bins` fills that
/// directory's `.bin`. Hoisted has no per-dep directory: transitives share
/// a `node_modules/` with everything else hoisted to that level, and the
/// only pass covering that directory handled the importer's *direct* deps.
/// A hoisted transitive's bins were therefore linked nowhere, and any
/// lifecycle script invoking one exited 127 (#570 — `bufferutil`'s
/// `install` script shells out to `node-gyp-build`).
///
/// Mirrors pnpm's hoisted layout, which runs `linkBins(modulesDir,
/// binsDir)` once per `node_modules/` in the tree and links the bins of
/// every package sitting directly inside it.
///
/// ORDERING is the conflict resolution. This pass runs before the
/// direct-dep and self-bin passes, whose writes then overwrite it
/// (`create_bin_shim` unlinks first), reproducing pnpm's
/// `preferDirectCmds` — a direct dep's bin beats a hoisted transitive's —
/// without a separate conflict table. Among colliding transitives,
/// `HoistedPlacements::iter` yields dep_paths in `BTreeMap` order, so the
/// greatest dep_path wins — deterministic across runs and platforms.
/// That matches only the MIDDLE tier of pnpm's `compareCommandsInConflict`
/// (`pkgOwnsBin` first, then `pkgName.localeCompare`, then `semver`), so a
/// collision where one package's bin is named after the package itself —
/// the common CLI-tool shape — can still pick the other one. Closing that
/// gap needs a real conflict table, not a write order.
#[allow(clippy::too_many_arguments)]
fn link_hoisted_placement_bins(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: &aube_linker::HoistedPlacements,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    for (dep_path, pkg_dir) in placements.iter() {
        let Some(pkg) = graph.get_package(dep_path) else {
            continue;
        };
        let pkg_json = read_materialized_pkg_json_cached(
            cache,
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            Some(placements),
        )?;
        let bin_dir = dep_modules_dir_for(pkg_dir, &pkg.name).join(".bin");
        link_bins_of_pkg_dir(
            &bin_dir,
            pkg_dir,
            &pkg.name,
            pkg_json.as_ref(),
            graph,
            dep_path,
            shim_opts,
            managed,
            preserved,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn link_bins(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
    ws_dirs: Option<&BTreeMap<String, PathBuf>>,
    ws_cache: &mut WsPkgJsonCache,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let bin_dir = project_dir.join(modules_dir_name).join(".bin");
    std::fs::create_dir_all(&bin_dir).into_diagnostic()?;

    for dep in graph.root_deps() {
        if let Some(ws_dir) = ws_dirs.and_then(|m| m.get(&dep.name)) {
            link_bins_from_dir(
                ws_cache, &bin_dir, ws_dir, &dep.name, shim_opts, managed, preserved,
            )?;
        } else if let Some(dir) = symlinked_dep_dir(graph, &dep.dep_path, project_dir) {
            link_bins_from_dir(
                ws_cache, &bin_dir, &dir, &dep.name, shim_opts, managed, preserved,
            )?;
        } else {
            link_bins_for_dep(
                cache,
                aube_dir,
                &bin_dir,
                graph,
                &dep.dep_path,
                &dep.name,
                virtual_store_dir_max_length,
                placements,
                shim_opts,
                managed,
                preserved,
            )?;
        }
    }

    Ok(())
}

/// Link bins declared by a dep that lives at a directory on disk rather
/// than in the virtual store — a `workspace:` sibling, or a `link:` /
/// `portal:` target. The linker symlinks all of these straight into the
/// importer's `node_modules/` and never materializes a
/// `.aube/<dep_path>/`, so `link_bins_for_dep` looks up a path that does
/// not exist and silently shims nothing. Read the package's own
/// `package.json` from `pkg_dir` instead and shim each bin entry, which
/// is what pnpm does for every one of these kinds.
///
/// `cache` deduplicates the read+parse across importers — without it,
/// a popular tooling package consumed by N workspace members gets its
/// `package.json` read N times during a single install.
pub(super) fn link_bins_from_dir(
    cache: &mut WsPkgJsonCache,
    bin_dir: &Path,
    pkg_dir: &Path,
    name: &str,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let pkg_json = if let Some(cached) = cache.get(pkg_dir) {
        cached.clone()
    } else {
        let pkg_json_path = pkg_dir.join("package.json");
        let parsed = match std::fs::read_to_string(&pkg_json_path) {
            Ok(content) => Some(
                aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
                    .map_err(miette::Report::new)
                    .wrap_err_with(|| {
                        format!("failed to parse package.json for local dep {name}")
                    })?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for local dep {name} at {}: {e}",
                    pkg_json_path.display()
                ));
            }
        };
        cache.insert(pkg_dir.to_path_buf(), parsed.clone());
        parsed
    };
    if let Some(pkg_json) = pkg_json {
        if let Some(bin) = pkg_json.get("bin") {
            link_bin_entries(
                bin_dir,
                pkg_dir,
                Some(name),
                bin,
                shim_opts,
                managed,
                preserved,
            )?;
        } else if let Some(dir_bin) = pkg_json.get("directories").and_then(|d| d.get("bin")) {
            link_dir_bins(bin_dir, pkg_dir, dir_bin, shim_opts, managed, preserved)?;
        }
    }
    Ok(())
}

/// On-disk root of a dep the linker SYMLINKED rather than materialized
/// into the virtual store, i.e. `link:` and `portal:`. `file:<dir>` is
/// deliberately absent: that kind is hardlink-copied into
/// `.aube/<dep_path>/`, so `materialized_pkg_dir` already finds it and
/// its bins work today.
///
/// Paths on `LocalSource` are stored relative to the project root.
fn symlinked_dep_dir(
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    root_dir: &Path,
) -> Option<PathBuf> {
    match graph.packages.get(dep_path)?.local_source.as_ref()? {
        aube_lockfile::LocalSource::Link(p) | aube_lockfile::LocalSource::Portal(p) => {
            Some(root_dir.join(p))
        }
        _ => None,
    }
}

/// Gate + run the per-dep `.bin` linking pass.
///
/// The link site in `run_link_phase` and the lifecycle site in
/// `run_finalize_phase` MUST agree on whether dep build scripts may run:
/// the link side shims each dep's children into its `.bin`, the lifecycle
/// side runs the dep's scripts with that `.bin` on PATH. The shared
/// [`super::default_trust::dep_build_scripts_may_run`] predicate is the
/// single source of truth — keeping the *gate decision itself* in this
/// function (rather than open-coded at the `run_link_phase` call site)
/// is what makes it directly testable against a real fixture graph: a
/// trust-floor-only install (`floor_may_allow_any && !has_any_allow_rule`)
/// must still link the bins, because the scripts run on the floor and
/// need their own deps' CLIs (e.g. lmdb's
/// `node-gyp-build-optional-packages`) on PATH.
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_link_dep_bins(
    ignore_scripts: bool,
    has_any_allow_rule: bool,
    floor_may_allow_any: bool,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    if !super::default_trust::dep_build_scripts_may_run(
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
    ) {
        return Ok(());
    }
    link_dep_bins(LinkDepBinsInput {
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        cache,
        managed,
        preserved,
    })
}

/// Write per-dep `.bin/` directories holding shims for each package's
/// *own* declared dependencies. Mirrors pnpm's post-link pass that
/// populates `node_modules/.pnpm/<dep_path>/node_modules/.bin/`.
///
/// Without this, a dep's lifecycle script (e.g. `unrs-resolver`'s
/// postinstall that calls `prebuild-install`) can't find transitive
/// binaries on PATH — the project-level `node_modules/.bin` only holds
/// shims for the root's *direct* deps. `run_dep_hook` walks the enclosing
/// `.bin` chain closest-first, so the dep's own transitive bins win.
///
/// Isolated mode only. Under hoisted, `link_hoisted_placement_bins` already
/// puts every placed package's bins in the `.bin` beside it, which is where
/// Node resolves that copy from, and `run_dep_hook`'s chain walk reaches
/// every one of those directories. Running this pass under hoisted would
/// instead write a nested child's bins into the *enclosing* (often root)
/// `.bin` — a shared directory whose contents are decided by pass order
/// inside `link_all_bins`, which standalone callers like `rebuild` do not
/// reproduce. Skipping keeps that directory owned by the ordered passes.
pub(crate) fn link_dep_bins(input: LinkDepBinsInput<'_>) -> miette::Result<()> {
    let LinkDepBinsInput {
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        cache,
        managed,
        preserved,
    } = input;
    if placements.is_some() {
        // Hoisted — skip. See function doc.
        return Ok(());
    }
    for (dep_path, pkg) in &graph.packages {
        if pkg.dependencies.is_empty() {
            continue;
        }
        let pkg_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !pkg_dir.exists() {
            // Filtered by optional / platform guards, or a staging
            // hiccup. Skipping avoids blowing up the whole install on
            // a dep that was never materialized.
            continue;
        }
        let dep_modules_dir = dep_modules_dir_for(&pkg_dir, &pkg.name);
        let bin_dir = dep_modules_dir.join(".bin");
        // Don't `create_dir_all(&bin_dir)` here — most deps have
        // no child that ships a `bin`, and an eager mkdir would leave
        // empty `.bin/` directories everywhere. `create_bin_link`
        // materializes the parent the first time a shim actually
        // lands, so deps whose children contribute zero shims stay
        // empty on disk.

        for (child_name, child_version) in &pkg.dependencies {
            // Resolve the edge to its graph key across reader conventions —
            // in lockstep with the linker (a raw `name@tail` doubled yarn's
            // full-dep_path values, so a yarn transitive dep's bins never got
            // linked). `None` = not a real graph node; skip.
            let Some(child_dep_path) =
                aube_lockfile::resolve_dep_edge(child_name, child_version, |k| {
                    graph.packages.contains_key(k)
                })
            else {
                continue;
            };
            // Mirror the linker's self-ref guard from `materialize_into`: a
            // package that depends on its own dep_path is a graph artefact.
            if child_dep_path == *dep_path && child_name == &pkg.name {
                continue;
            }
            // The sibling may have been filtered (optional on another
            // platform); `link_bins_for_dep` already returns Ok when
            // the target pkg_json is absent, so just call through.
            link_bins_for_dep(
                cache,
                aube_dir,
                &bin_dir,
                graph,
                &child_dep_path,
                child_name,
                virtual_store_dir_max_length,
                placements,
                shim_opts,
                managed,
                preserved,
            )?;
        }
    }
    Ok(())
}

/// Link every bin surface exposed by an install.
///
/// This runs before dependency lifecycle scripts so builds can invoke their
/// dependencies, then again after approved builds. The second pass refreshes
/// packages whose lifecycle replaces a bin target.
pub(super) fn link_all_bins(input: LinkAllBinsInput<'_>) -> miette::Result<ManagedBinLinks> {
    let LinkAllBinsInput {
        project_dir,
        settings_ctx,
        modules_dir_name,
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        ws_dirs,
        manifests,
        manifest,
        node_linker,
        has_workspace,
        virtual_store_only,
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
        preserved,
    } = input;

    if virtual_store_only {
        return Ok(ManagedBinLinks::default());
    }
    let extend_node_path = aube_settings::resolved::extend_node_path(settings_ctx);
    let isolated = !matches!(node_linker, aube_linker::NodeLinker::Hoisted);
    let prefer_symlinked_executables =
        aube_settings::resolved::prefer_symlinked_executables(settings_ctx)
            .or(isolated.then_some(false));
    let hidden_modules_dir = aube_dir.join("node_modules");
    let shim_opts = aube_linker::BinShimOptions {
        extend_node_path,
        prefer_symlinked_executables,
        hidden_modules_dir: isolated.then_some(hidden_modules_dir.as_path()),
    };

    let mut pkg_json_cache = PkgJsonCache::new();
    let mut ws_pkg_json_cache = WsPkgJsonCache::new();
    let mut managed = if super::default_trust::dep_build_scripts_may_run(
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
    ) {
        ManagedBinLinks::capturing()
    } else {
        ManagedBinLinks::default()
    };
    let ws_dirs_for_bins = has_workspace.then_some(ws_dirs);
    // Writers into a SHARED `.bin` run lowest-precedence first, because every
    // later pass overwrites a same-named shim (`create_bin_shim` unlinks
    // before it writes). That makes the order below the whole conflict
    // resolution for the hoisted layout:
    //   hoisted placements < direct deps < self-bin.
    // `link_dep_bins` is deliberately NOT part of this sequence — it is
    // isolated-only, and its per-dep targets are disjoint from every `.bin`
    // written here, so it stays at the end.
    if let Some(placements) = placements {
        link_hoisted_placement_bins(
            aube_dir,
            graph,
            virtual_store_dir_max_length,
            placements,
            shim_opts,
            &mut pkg_json_cache,
            &mut managed,
            preserved,
        )?;
    }
    link_bins(
        project_dir,
        modules_dir_name,
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        &mut pkg_json_cache,
        ws_dirs_for_bins,
        &mut ws_pkg_json_cache,
        &mut managed,
        preserved,
    )?;

    // Root self-bins override dependency bins with the same name. Force a
    // wrapper because generated output may not exist yet or be executable.
    if let Some(bin) = manifest.extra.get("bin") {
        let root_bin_dir = project_dir.join(modules_dir_name).join(".bin");
        let self_shim_opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..shim_opts
        };
        link_bin_entries(
            &root_bin_dir,
            project_dir,
            manifest.name.as_deref(),
            bin,
            self_shim_opts,
            &mut managed,
            preserved,
        )?;
    }

    if has_workspace {
        for (importer_path, deps) in &graph.importers {
            if importer_path == "." || !aube_linker::is_physical_importer(importer_path) {
                continue;
            }
            let pkg_dir = project_dir.join(importer_path);
            let bin_dir = pkg_dir.join(modules_dir_name).join(".bin");
            std::fs::create_dir_all(&bin_dir).into_diagnostic()?;
            for dep in deps {
                if let Some(ws_dir) = ws_dirs.get(&dep.name) {
                    link_bins_from_dir(
                        &mut ws_pkg_json_cache,
                        &bin_dir,
                        ws_dir,
                        &dep.name,
                        shim_opts,
                        &mut managed,
                        preserved,
                    )?;
                } else if let Some(dir) = symlinked_dep_dir(graph, &dep.dep_path, project_dir) {
                    link_bins_from_dir(
                        &mut ws_pkg_json_cache,
                        &bin_dir,
                        &dir,
                        &dep.name,
                        shim_opts,
                        &mut managed,
                        preserved,
                    )?;
                } else {
                    link_bins_for_dep(
                        &mut pkg_json_cache,
                        aube_dir,
                        &bin_dir,
                        graph,
                        &dep.dep_path,
                        &dep.name,
                        virtual_store_dir_max_length,
                        placements,
                        shim_opts,
                        &mut managed,
                        preserved,
                    )?;
                }
            }
            if let Some((_, member_manifest)) =
                manifests.iter().find(|(path, _)| path == importer_path)
                && let Some(bin) = member_manifest.extra.get("bin")
            {
                let self_shim_opts = aube_linker::BinShimOptions {
                    prefer_symlinked_executables: Some(false),
                    ..shim_opts
                };
                link_bin_entries(
                    &bin_dir,
                    &pkg_dir,
                    member_manifest.name.as_deref(),
                    bin,
                    self_shim_opts,
                    &mut managed,
                    preserved,
                )?;
            }
        }
    }

    maybe_link_dep_bins(
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        &mut pkg_json_cache,
        &mut managed,
        preserved,
    )?;
    Ok(managed)
}

/// Remove only shims that still match entries created by the pre-build pass.
/// Lifecycle-produced files or retargeted symlinks are left untouched.
pub(crate) fn remove_managed_bin_links(
    managed: &ManagedBinLinks,
) -> miette::Result<PreservedBinLinks> {
    let mut preserved = PreservedBinLinks::new();
    for (bin_dir, entries) in &managed.entries {
        for (name, expected_files) in entries {
            let mut matching = Vec::new();
            let mut replaced = false;
            for (path, expected) in expected_files {
                match read_managed_bin_entry(path)? {
                    Some(current) if current == *expected => matching.push(path),
                    Some(_) | None => replaced = true,
                }
            }
            if replaced {
                preserved
                    .entry(bin_dir.clone())
                    .or_default()
                    .insert(name.clone());
            } else {
                // A command can be a family of launchers on Windows
                // (`name`, `name.cmd`, and `name.ps1`). If a lifecycle
                // script replaces any member, keep the unchanged siblings
                // too: the relink pass preserves the whole command, and
                // deleting only its matching members would make it
                // unavailable from some shells.
                for path in matching {
                    std::fs::remove_file(path).into_diagnostic()?;
                }
            }
        }
    }
    Ok(preserved)
}

/// Remove preserved command families that are no longer declared by the
/// post-lifecycle package manifests. Commands encountered by the relink pass
/// stay preserved, including any intentionally replaced or deleted launcher.
pub(crate) fn remove_unclaimed_preserved_bin_links(
    managed: &ManagedBinLinks,
    preserved: &PreservedBinLinks,
    relinked: &ManagedBinLinks,
) -> miette::Result<()> {
    for (bin_dir, names) in preserved {
        for name in names {
            if relinked
                .seen
                .get(bin_dir)
                .is_some_and(|seen| seen.contains(name))
            {
                continue;
            }
            let Some(expected_files) = managed
                .entries
                .get(bin_dir)
                .and_then(|entries| entries.get(name))
            else {
                continue;
            };
            for path in expected_files.keys() {
                match std::fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                        std::fs::remove_dir_all(path).into_diagnostic()?;
                    }
                    Ok(_) => std::fs::remove_file(path).into_diagnostic()?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).into_diagnostic(),
                }
            }
        }
    }
    Ok(())
}

fn read_managed_bin_entry(path: &Path) -> miette::Result<Option<ManagedBinEntry>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).into_diagnostic(),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::read_link(path)
            .map(ManagedBinEntry::Symlink)
            .map(Some)
            .into_diagnostic();
    }
    if metadata.is_file() {
        return std::fs::read(path)
            .map(ManagedBinEntry::File)
            .map(Some)
            .into_diagnostic();
    }
    Ok(Some(ManagedBinEntry::Other))
}

fn bin_link_paths(bin_dir: &Path, name: &str) -> Vec<PathBuf> {
    let link = bin_dir.join(name);
    #[cfg(windows)]
    return vec![
        link,
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.ps1")),
    ];
    #[cfg(not(windows))]
    vec![link]
}

/// Hoist bins declared by a package's `bundledDependencies` into
/// `bin_dir`. The bundled children live under
/// `<pkg_dir>/node_modules/<bundled>/` straight from the tarball — the
/// resolver never walks them, so they don't show up in the regular
/// packument-driven bin-linking pass and need this companion hoist.
/// Matches pnpm's post-bin-linking pass for `hasBundledDependencies`.
/// Used by both the root importer (`link_bins`) and the per-workspace
/// loop so a workspace package depending on a parent with bundled deps
/// sees the children's bins in its own `node_modules/.bin`.
fn link_bundled_bins(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let Some(locked) = graph.get_package(dep_path) else {
        return Ok(());
    };
    for bundled in &locked.bundled_dependencies {
        let bundled_dir = pkg_dir.join("node_modules").join(bundled);
        let bundled_pkg_json_path = bundled_dir.join("package.json");
        let Ok(content) = std::fs::read_to_string(&bundled_pkg_json_path) else {
            continue;
        };
        let Ok(bundled_pkg_json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(bin) = bundled_pkg_json.get("bin") {
            link_bin_entries(
                bin_dir,
                &bundled_dir,
                Some(bundled),
                bin,
                shim_opts,
                managed,
                preserved,
            )?;
        } else if let Some(dir_bin) = bundled_pkg_json
            .get("directories")
            .and_then(|d| d.get("bin"))
        {
            link_dir_bins(
                bin_dir,
                &bundled_dir,
                dir_bin,
                shim_opts,
                managed,
                preserved,
            )?;
        }
    }
    Ok(())
}

/// Shim each entry of a package.json `bin` field into `bin_dir`,
/// resolving relative targets against `pkg_dir`. Shared by the
/// dep-bin pass (`link_bins_for_dep`), bundled-deps pass
/// (`link_bundled_bins`), and importer self-bin pass (root + each
/// workspace member, discussion #228).
///
/// String-form `bin: "./x.js"` uses the basename of `pkg_name` as the
/// shim name (scope `@a/b` → `b`); the entry is silently skipped when
/// `pkg_name` is `None`. Object-form `bin: { foo: "./f" }` uses each
/// key as-is. Entries whose name or target fail
/// [`aube_linker::validate_bin_name`] / [`aube_linker::validate_bin_target`]
/// are dropped without error, matching the pnpm/npm "silently ignore
/// invalid bin" behavior.
pub(super) fn link_bin_entries(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    pkg_name: Option<&str>,
    bin: &serde_json::Value,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    match bin {
        serde_json::Value::String(bin_path) => {
            let Some(name) = pkg_name else {
                return Ok(());
            };
            let bin_name = name.split('/').next_back().unwrap_or(name);
            if aube_linker::validate_bin_name(bin_name).is_ok()
                && aube_linker::validate_bin_target(bin_path).is_ok()
            {
                create_bin_link(
                    bin_dir,
                    bin_name,
                    &pkg_dir.join(bin_path),
                    shim_opts,
                    managed,
                    preserved,
                )?;
            }
        }
        serde_json::Value::Object(bins) => {
            for (bin_name, path) in bins {
                if let Some(path_str) = path.as_str()
                    && aube_linker::validate_bin_name(bin_name).is_ok()
                    && aube_linker::validate_bin_target(path_str).is_ok()
                {
                    create_bin_link(
                        bin_dir,
                        bin_name,
                        &pkg_dir.join(path_str),
                        shim_opts,
                        managed,
                        preserved,
                    )?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Fallback bin-linking for a package that declares no top-level
/// `bin` but DOES set `directories.bin`. Matches the dominant
/// reference PMs (npm 11, pnpm 10, yarn-classic, bun all link it;
/// only yarn-berry ignores it) and pnpm's exact rule in
/// `bins/resolver/src/index.ts`: when `bin` is absent and
/// `directories.bin` is present, every regular FILE under that
/// directory (recursive, files only, symlinks NOT followed) becomes a
/// bin named `basename(file)`.
///
/// `bin` always wins — this is invoked ONLY when `bin` is absent, so a
/// package shipping both is unaffected (npm errors on both anyway).
///
/// CONTAINMENT: `directories.bin` is a package-authored relative path,
/// so it is the attack surface. Two guards keep every shim inside the
/// package:
///
/// 1. the dir itself must canonicalize to a path inside `pkg_dir`
///    (rejects `../escape`, absolute, symlink-to-elsewhere) — pnpm's
///    `isSubdir(pkgPath, binDir)` check;
/// 2. each discovered file's basename is run through
///    [`aube_linker::validate_bin_name`], dropping anything unsafe,
///    same as the `bin`-field path.
///
/// The walk does not follow symlinks, so a symlinked subdir can't pull
/// the walk outside the tree.
fn link_dir_bins(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    directories_bin: &serde_json::Value,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    let Some(rel) = directories_bin.as_str() else {
        return Ok(());
    };
    if rel.is_empty() {
        return Ok(());
    }

    let bins_root = pkg_dir.join(rel);

    // Containment guard 1: the bin dir must resolve within the package
    // root. Canonicalize both sides so a `directories.bin` of `../x`,
    // an absolute path, or a symlink pointing elsewhere is rejected. If
    // the dir doesn't exist (`canonicalize` errors) there's nothing to
    // link — silently skip, matching pnpm's ENOENT-swallow in
    // `findFiles`.
    let (Ok(canon_root), Ok(canon_bins)) = (
        std::fs::canonicalize(pkg_dir),
        std::fs::canonicalize(&bins_root),
    ) else {
        return Ok(());
    };
    // Reject both an escape (`../x`, absolute, symlink-elsewhere) AND
    // the package root itself: pnpm's `isSubdir(pkgPath, binDir)` is
    // false when the two are equal, so `directories.bin: "."` links
    // NOTHING rather than shimming every file in the package.
    if canon_bins == canon_root || !canon_bins.starts_with(&canon_root) {
        return Ok(());
    }

    let mut files = Vec::new();
    collect_files_no_symlink_follow(&bins_root, &mut files)?;
    // Stable order so the result is deterministic regardless of
    // readdir order across platforms.
    files.sort();

    for file in &files {
        let Some(file_name) = file.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Containment guard 2: drop any basename that isn't a safe bin
        // name (path separators, traversal, control chars, …). Same
        // gate the `bin`-field path applies.
        if aube_linker::validate_bin_name(file_name).is_err() {
            continue;
        }
        create_bin_link(bin_dir, file_name, file, shim_opts, managed, preserved)?;
    }
    Ok(())
}

/// Recursively collect regular files under `dir`, NOT following
/// symlinks (mirrors pnpm's `glob('**', { onlyFiles: true,
/// followSymbolicLinks: false })`). A symlinked subdirectory is left
/// unvisited, so the walk can never escape `dir`'s real subtree. A
/// symlinked file is skipped (it is not a regular file by
/// `is_file()` on the symlink's own metadata via `symlink_metadata`).
fn collect_files_no_symlink_follow(
    dir: &std::path::Path,
    out: &mut Vec<PathBuf>,
) -> miette::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // The dir was guaranteed to exist by the canonicalize check in
        // the caller, but a concurrent removal could still race; treat
        // a vanished dir as empty rather than failing the install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(miette!(
                "failed to read directories.bin dir {}: {e}",
                dir.display()
            ));
        }
    };
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        // `symlink_metadata` does NOT follow the final component, so a
        // symlink is classified by its own type — a symlinked dir is
        // not a dir here (skipped, no recursion), a symlinked file is
        // not a regular file (skipped).
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(miette!("failed to stat {}: {e}", path.display()));
            }
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            collect_files_no_symlink_follow(&path, out)?;
        } else if ft.is_file() {
            out.push(path);
        }
        // symlinks (is_symlink) and other special files are skipped.
    }
    Ok(())
}

fn create_bin_link(
    bin_dir: &std::path::Path,
    name: &str,
    target: &std::path::Path,
    shim_opts: aube_linker::BinShimOptions,
    managed: &mut ManagedBinLinks,
    preserved: Option<&PreservedBinLinks>,
) -> miette::Result<()> {
    if let Some(preserved) = preserved {
        managed
            .seen
            .entry(bin_dir.to_path_buf())
            .or_default()
            .insert(name.to_string());
        if preserved
            .get(bin_dir)
            .is_some_and(|names| names.contains(name))
        {
            return Ok(());
        }
    }
    // `link_dep_bins` skips eager `create_dir_all` on per-dep `.bin/`.
    // Deps whose children ship no bins stay empty on disk. First shim
    // write materializes the dir on demand.
    //
    // Windows `CreateDirectoryW` returns `ERROR_ALREADY_EXISTS` (os 183)
    // when the leaf sits behind a junction in the path, even when the
    // leaf is absent. The isolated layout's `.aube/<dep_path>` is a
    // junction into the global virtual store, so every `.bin/` under it
    // hits the quirk. Workaround: canonicalize the parent
    // (`crate::dirs::canonicalize` already strips the `\\?\` verbatim
    // prefix, which would otherwise trip CreateDirectoryW's own os-123
    // quirk, while keeping real `\\?\UNC\…` share paths intact), then
    // create everything down to `link_path.parent()` on that plain-drive
    // root. The leaf inode is shared with the surface side, so
    // `create_bin_shim` later writes through the surface path into the
    // same directory. Including the `link_path.parent()` here covers
    // scoped bin names (`@scope/foo`): we have to pre-create
    // `<bin_dir>/@scope/` on the canonical side too, because
    // `create_bin_shim`'s own `create_dir_all` would otherwise trip the
    // same quirk on the surface side and the shim's `@scope/foo.cmd`
    // write would fail with `NotFound`. No-op on Unix.
    //
    // Pass the *surface* `bin_dir` (not the canonicalized form) to
    // `create_bin_shim`: the shim's relative target is anchored on
    // `link_parent`, and the canonical form lives on a different
    // subtree (the GVS, e.g. `…\aube\virtual-store\…`) than the
    // surface invocation path (`…\.aube\<dep_path>\node_modules\.bin\`).
    // `pathdiff` would then find only `C:\Users\…\AppData\Local\` as a
    // common prefix and emit a long `..\..\..\…` traversal back down
    // through the surface tree, producing the duplicated install-root
    // path Node surfaces as `Cannot find module
    // '…\pnpm\global-aube\<hash>\pnpm\global-aube\<hash>\…'`
    // (Discussion #654).
    #[cfg(windows)]
    let mkdir_root_owned = bin_dir.parent().and_then(|parent| {
        let leaf = bin_dir.file_name()?;
        let canon = crate::dirs::canonicalize(parent).ok()?;
        Some(canon.join(leaf))
    });
    #[cfg(windows)]
    let mkdir_root: &std::path::Path = mkdir_root_owned.as_deref().unwrap_or(bin_dir);
    #[cfg(not(windows))]
    let mkdir_root = bin_dir;
    let mkdir_link_path = mkdir_root.join(name);
    let mkdir_target = mkdir_link_path.parent().unwrap_or(mkdir_root);
    if let Err(e) = std::fs::create_dir_all(mkdir_target) {
        let tolerated = e.kind() == std::io::ErrorKind::AlreadyExists && mkdir_target.is_dir();
        if !tolerated {
            return Err(e)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create bin directory {}", bin_dir.display()));
        }
    }
    aube_linker::create_bin_shim(bin_dir, name, target, shim_opts)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to link bin `{name}` at {} -> {}",
                bin_dir.join(name).display(),
                target.display()
            )
        })?;
    if !managed.capture {
        return Ok(());
    }
    let mut files = BTreeMap::new();
    for path in bin_link_paths(bin_dir, name) {
        if let Some(entry) = read_managed_bin_entry(&path)? {
            files.insert(path, entry);
        }
    }
    managed
        .entries
        .entry(bin_dir.to_path_buf())
        .or_default()
        .insert(name.to_string(), files);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};

    fn locked(name: &str, version: &str, bin: BTreeMap<String, String>) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            bin,
            ..Default::default()
        }
    }

    /// Materialize a parent dep that declares one child dep shipping a
    /// `bin`, then return everything `maybe_link_dep_bins` needs plus the
    /// path where the child's shim must land. The shape mirrors lmdb (the
    /// parent, whose postinstall shells out to a dep CLI) depending on
    /// `node-gyp-build-optional-packages` (the child that ships the CLI).
    fn fixture_parent_with_bin_bearing_child(
        aube_dir: &std::path::Path,
    ) -> (LockfileGraph, std::path::PathBuf) {
        let parent_dep_path = "lmdb@3.0.0";
        let child_dep_path = "node-gyp-build-optional-packages@5.2.0";

        // Parent on disk (must exist or `link_dep_bins` skips it).
        let parent_dir = materialized_pkg_dir(aube_dir, parent_dep_path, "lmdb", 120, None);
        std::fs::create_dir_all(&parent_dir).unwrap();
        std::fs::write(
            parent_dir.join("package.json"),
            r#"{"name":"lmdb","version":"3.0.0"}"#,
        )
        .unwrap();

        // Child on disk, declaring a bin + the target file the shim points at.
        let child_dir = materialized_pkg_dir(
            aube_dir,
            child_dep_path,
            "node-gyp-build-optional-packages",
            120,
            None,
        );
        std::fs::create_dir_all(child_dir.join("bin")).unwrap();
        std::fs::write(
            child_dir.join("package.json"),
            r#"{"name":"node-gyp-build-optional-packages","version":"5.2.0","bin":{"node-gyp-build-optional-packages":"bin/build.js"}}"#,
        )
        .unwrap();
        std::fs::write(child_dir.join("bin/build.js"), "#!/usr/bin/env node\n").unwrap();

        let mut parent = locked("lmdb", "3.0.0", BTreeMap::new());
        parent.dependencies.insert(
            "node-gyp-build-optional-packages".to_string(),
            "5.2.0".to_string(),
        );

        let mut packages = BTreeMap::new();
        packages.insert(parent_dep_path.to_string(), parent);
        packages.insert(
            child_dep_path.to_string(),
            locked("node-gyp-build-optional-packages", "5.2.0", {
                let mut b = BTreeMap::new();
                b.insert(
                    "node-gyp-build-optional-packages".to_string(),
                    "bin/build.js".to_string(),
                );
                b
            }),
        );

        let graph = LockfileGraph {
            packages,
            ..Default::default()
        };

        // Where the child's shim must land: in the PARENT's per-dep `.bin`,
        // so lmdb's postinstall finds it on PATH.
        let expected_shim = dep_modules_dir_for(&parent_dir, "lmdb")
            .join(".bin")
            .join("node-gyp-build-optional-packages");
        (graph, expected_shim)
    }

    /// THE LINK-SITE TRANSITIVE-BIN REGRESSION. `maybe_link_dep_bins` is
    /// the gate `run_link_phase` calls — it must shim a dep's child bins
    /// whenever those scripts *may* run, which on a pure trust-floor
    /// install means `floor_may_allow_any && !has_any_allow_rule`. The
    /// pre-fix gate checked `has_any_allow_rule` only, so this exact
    /// shape (lmdb on the `defaultTrust` floor, no explicit `allowBuilds`)
    /// ran lmdb's postinstall but never linked
    /// `node-gyp-build-optional-packages` into lmdb's `.bin` → exit 127.
    ///
    /// Load-bearing against re-drift: reverting the call-site gate to
    /// `has_any_allow_rule()`-only flips the floor-only case below from
    /// "shim present" to "shim absent" and FAILS this test. (The helper-
    /// in-isolation test in `default_trust.rs` does NOT — it never reaches
    /// `link_dep_bins`.)
    #[test]
    fn maybe_link_dep_bins_links_transitive_bins_on_a_pure_trust_floor() {
        // Trust-floor-only: no allow rule, but the floor could authorize a
        // build → scripts run → their deps' bins MUST be linked.
        let dir = tempfile::tempdir().unwrap();
        let aube_dir = dir.path().join("node_modules/.aube");
        let (graph, expected_shim) = fixture_parent_with_bin_bearing_child(&aube_dir);
        maybe_link_dep_bins(
            /* ignore_scripts */ false,
            /* has_any_allow_rule */ false,
            /* floor_may_allow_any */ true,
            &aube_dir,
            &graph,
            120,
            None,
            aube_linker::BinShimOptions::default(),
            &mut PkgJsonCache::new(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();
        assert!(
            expected_shim.exists(),
            "trust-floor-only install must link the dep's transitive bin \
             into the parent's .bin so its postinstall finds it on PATH; \
             expected shim at {}",
            expected_shim.display()
        );

        // Fast-path: nothing may run (no allow rule, floor closed,
        // scripts not ignored) → the pass is skipped, no shim written.
        let dir2 = tempfile::tempdir().unwrap();
        let aube_dir2 = dir2.path().join("node_modules/.aube");
        let (graph2, expected_shim2) = fixture_parent_with_bin_bearing_child(&aube_dir2);
        maybe_link_dep_bins(
            false,
            false,
            false,
            &aube_dir2,
            &graph2,
            120,
            None,
            aube_linker::BinShimOptions::default(),
            &mut PkgJsonCache::new(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();
        assert!(
            !expected_shim2.exists(),
            "with no allow rule and the floor closed, no scripts run, so the \
             dep-bin pass must be skipped (fast path) — no shim should appear"
        );
    }

    #[test]
    fn managed_bin_cleanup_removes_owned_shims_and_preserves_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let removed_target = pkg_dir.join("removed.js");
        let replaced_target = pkg_dir.join("replaced.js");
        std::fs::write(&removed_target, "#!/usr/bin/env node\n").unwrap();
        std::fs::write(&replaced_target, "#!/usr/bin/env node\n").unwrap();

        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        let mut managed = ManagedBinLinks::capturing();
        create_bin_link(
            &bin_dir,
            "removed",
            &removed_target,
            opts,
            &mut managed,
            None,
        )
        .unwrap();
        create_bin_link(
            &bin_dir,
            "replaced",
            &replaced_target,
            opts,
            &mut managed,
            None,
        )
        .unwrap();

        std::fs::write(bin_dir.join("replaced"), "#!/bin/sh\necho custom\n").unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        create_bin_link(
            &bin_dir,
            "replaced",
            &replaced_target,
            opts,
            &mut ManagedBinLinks::default(),
            Some(&preserved),
        )
        .unwrap();

        assert!(!bin_dir.join("removed").exists());
        assert_eq!(
            std::fs::read_to_string(bin_dir.join("replaced")).unwrap(),
            "#!/bin/sh\necho custom\n"
        );
    }

    #[test]
    fn managed_bin_cleanup_preserves_siblings_of_a_replaced_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let launcher = bin_dir.join("tool");
        let sibling = bin_dir.join("tool.cmd");
        std::fs::write(&launcher, "generated launcher\n").unwrap();
        std::fs::write(&sibling, "generated sibling\n").unwrap();

        let mut expected_files = BTreeMap::new();
        expected_files.insert(
            launcher.clone(),
            read_managed_bin_entry(&launcher).unwrap().unwrap(),
        );
        expected_files.insert(
            sibling.clone(),
            read_managed_bin_entry(&sibling).unwrap().unwrap(),
        );
        let mut commands = BTreeMap::new();
        commands.insert("tool".to_string(), expected_files);
        let mut managed = ManagedBinLinks::capturing();
        managed.entries.insert(bin_dir.clone(), commands);

        std::fs::write(&launcher, "lifecycle replacement\n").unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();

        assert!(preserved[&bin_dir].contains("tool"));
        assert_eq!(
            std::fs::read_to_string(launcher).unwrap(),
            "lifecycle replacement\n"
        );
        assert_eq!(
            std::fs::read_to_string(sibling).unwrap(),
            "generated sibling\n"
        );
    }

    #[test]
    fn managed_bin_cleanup_preserves_siblings_of_a_deleted_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let launcher = bin_dir.join("tool");
        let sibling = bin_dir.join("tool.cmd");
        std::fs::write(&launcher, "generated launcher\n").unwrap();
        std::fs::write(&sibling, "generated sibling\n").unwrap();

        let mut expected_files = BTreeMap::new();
        expected_files.insert(
            launcher.clone(),
            read_managed_bin_entry(&launcher).unwrap().unwrap(),
        );
        expected_files.insert(
            sibling.clone(),
            read_managed_bin_entry(&sibling).unwrap().unwrap(),
        );
        let mut commands = BTreeMap::new();
        commands.insert("tool".to_string(), expected_files);
        let mut managed = ManagedBinLinks::capturing();
        managed.entries.insert(bin_dir.clone(), commands);

        std::fs::remove_file(&launcher).unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        let mut relinked = ManagedBinLinks::default();
        create_bin_link(
            &bin_dir,
            "tool",
            dir.path().join("target.js").as_path(),
            Default::default(),
            &mut relinked,
            Some(&preserved),
        )
        .unwrap();
        remove_unclaimed_preserved_bin_links(&managed, &preserved, &relinked).unwrap();

        assert!(preserved[&bin_dir].contains("tool"));
        assert!(!launcher.exists());
        assert_eq!(
            std::fs::read_to_string(sibling).unwrap(),
            "generated sibling\n"
        );
    }

    #[test]
    fn post_lifecycle_relink_removes_deleted_bin_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "removes-bin@1.0.0";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "removes-bin", 120, None);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("cli.js"), "#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"removes-bin","version":"1.0.0","bin":{"removed-bin":"cli.js"}}"#,
        )
        .unwrap();

        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("removes-bin", "1.0.0", BTreeMap::new()),
        );
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "removes-bin".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        let mut managed = ManagedBinLinks::capturing();
        link_bins(
            project_dir,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            None,
            opts,
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut managed,
            None,
        )
        .unwrap();
        let shim = project_dir.join("node_modules/.bin/removed-bin");
        assert!(shim.exists());

        // Simulate an approved dependency lifecycle script removing its bin
        // declaration before the post-build refresh.
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"removes-bin","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::remove_file(&shim).unwrap();
        let preserved = remove_managed_bin_links(&managed).unwrap();
        let mut relinked = ManagedBinLinks::default();
        link_bins(
            project_dir,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            None,
            opts,
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut relinked,
            Some(&preserved),
        )
        .unwrap();
        remove_unclaimed_preserved_bin_links(&managed, &preserved, &relinked).unwrap();

        assert!(!shim.exists());
    }

    #[test]
    fn link_bins_reads_manifest_when_lockfile_metadata_is_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "vitepress@1.6.4";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "vitepress", 120, None);
        std::fs::create_dir_all(pkg_dir.join("bin")).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"vitepress","bin":{"vitepress":"bin/vitepress.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("bin/vitepress.js"), "#!/usr/bin/env node\n").unwrap();

        let mut semver_bin = BTreeMap::new();
        semver_bin.insert("semver".to_string(), "bin/semver.js".to_string());

        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("vitepress", "1.6.4", BTreeMap::new()),
        );
        packages.insert(
            "semver@7.7.4".to_string(),
            locked("semver", "7.7.4", semver_bin),
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "vitepress".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^1.5.0".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        link_bins(
            project_dir,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            None,
            aube_linker::BinShimOptions::default(),
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();

        assert!(project_dir.join("node_modules/.bin/vitepress").exists());
    }

    /// Regression for Discussion #654. The isolated layout puts
    /// `.aube/<dep_path>` as an NTFS junction into the global virtual
    /// store, and per-dep `.bin/` lives under that junction. The
    /// previous `create_bin_link` body canonicalized the bin-dir parent
    /// (workaround for `CreateDirectoryW`'s ERROR_ALREADY_EXISTS quirk)
    /// and *also* handed that canonical path to `create_bin_shim`. The
    /// generated `.cmd` then anchored its relative target on the GVS
    /// subtree, but `%~dp0` at runtime is the surface invocation path —
    /// so the combined path re-descended through the install root and
    /// Node surfaced `Cannot find module
    /// '…\pnpm\global-aube\<hash>\pnpm\global-aube\<hash>\…'`. The fix
    /// keeps the canonical mkdir but routes the shim writer through
    /// the surface `bin_dir`, so `pathdiff` sees a short common prefix
    /// and emits the expected `..\..\..\…` form.
    #[cfg(windows)]
    #[test]
    fn create_bin_link_surface_relative_path_when_dep_dir_is_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let aube_dir = project.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).unwrap();

        // Stand-in for the GVS: a separate subtree the dep_path junction
        // points at.
        let gvs = project.join("gvs");
        let gvs_dep = gvs.join("node-liblzma@2.2.0/node_modules");
        std::fs::create_dir_all(&gvs_dep).unwrap();
        aube_linker::create_dir_link(
            &gvs.join("node-liblzma@2.2.0"),
            &aube_dir.join("node-liblzma@2.2.0"),
        )
        .unwrap();

        // Sibling `.aube/` entry housing the bin we want to shim into
        // the junction's `.bin/`. Lives on the surface tree, not under
        // the junction.
        let target_pkg = aube_dir.join("prebuild-install@7.1.3/node_modules/prebuild-install");
        std::fs::create_dir_all(&target_pkg).unwrap();
        let target = target_pkg.join("bin.js");
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();

        // Surface bin dir: traverses the junction. Pre-fix, the canonical
        // form lived under `gvs/…`, which is precisely the mismatch this
        // test pins down.
        let bin_dir = aube_dir.join("node-liblzma@2.2.0/node_modules/.bin");

        create_bin_link(
            &bin_dir,
            "prebuild-install",
            &target,
            aube_linker::BinShimOptions::default(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();

        let cmd = std::fs::read_to_string(bin_dir.join("prebuild-install.cmd")).unwrap();
        // Three uplevels out of `.bin/`: `.bin` → `node_modules` →
        // `node-liblzma@2.2.0` → `.aube`, then descend into the sibling
        // `prebuild-install@7.1.3` entry.
        let expected = r"..\..\..\prebuild-install@7.1.3\node_modules\prebuild-install\bin.js";
        assert!(
            cmd.contains(expected),
            ".cmd shim should embed surface-tree relative path `{expected}`; got:\n{cmd}"
        );
        // Belt-and-braces: the pre-fix bug embedded a path that re-descended
        // through the project root after a long `..\` chain. Reject any
        // absolute-style fragment or a relative path that escapes far enough
        // to climb above `.aube/`.
        assert!(
            !cmd.contains(r"..\..\..\..\"),
            ".cmd shim should not climb above the `.aube/` root; got:\n{cmd}"
        );
    }

    /// Companion to the case above: scoped bin name (`@scope/foo`)
    /// behind the same junction. The pre-fix code routed shim writes
    /// through the canonical bin dir, so `create_bin_shim`'s internal
    /// `create_dir_all(<bin>\@scope)` ran on the GVS subtree where no
    /// junction is in the path — it just worked. With the fix, the
    /// shim writer sees the *surface* path and would hit the same
    /// "leaf behind junction" `ERROR_ALREADY_EXISTS` quirk on the
    /// `@scope/` mkdir. The fix's other half is pre-creating
    /// `link_path.parent()` on the canonical side; this test pins
    /// that behavior — without it, `@scope/foo.cmd` would fail to
    /// write through the junction with `NotFound`.
    #[cfg(windows)]
    #[test]
    fn create_bin_link_creates_scoped_parent_when_dep_dir_is_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let aube_dir = project.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).unwrap();

        let gvs = project.join("gvs");
        let gvs_dep = gvs.join("node-liblzma@2.2.0/node_modules");
        std::fs::create_dir_all(&gvs_dep).unwrap();
        aube_linker::create_dir_link(
            &gvs.join("node-liblzma@2.2.0"),
            &aube_dir.join("node-liblzma@2.2.0"),
        )
        .unwrap();

        // Scoped sibling: target lives at
        // `.aube/@scope+tool@1.0.0/node_modules/@scope/tool/cli.js` on
        // the surface tree (the linker escapes `/` as `+` in the
        // dep_path filename).
        let target_pkg = aube_dir.join("@scope+tool@1.0.0/node_modules/@scope/tool");
        std::fs::create_dir_all(&target_pkg).unwrap();
        let target = target_pkg.join("cli.js");
        std::fs::write(&target, "#!/usr/bin/env node\n").unwrap();

        let bin_dir = aube_dir.join("node-liblzma@2.2.0/node_modules/.bin");

        create_bin_link(
            &bin_dir,
            "@scope/tool",
            &target,
            aube_linker::BinShimOptions::default(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();

        // `@scope/` must exist as an actual directory on the surface
        // side (visible via the junction) so the shim file landed.
        assert!(
            bin_dir.join("@scope").is_dir(),
            "scoped parent `@scope/` should be pre-created through the junction"
        );
        let cmd = std::fs::read_to_string(bin_dir.join("@scope/tool.cmd")).unwrap();
        // Four uplevels out of `.bin/@scope/`: `@scope` → `.bin` →
        // `node_modules` → `node-liblzma@2.2.0` → `.aube`, then descend
        // into the sibling scoped entry.
        let expected = r"..\..\..\..\@scope+tool@1.0.0\node_modules\@scope\tool\cli.js";
        assert!(
            cmd.contains(expected),
            ".cmd shim should embed surface-tree relative path `{expected}`; got:\n{cmd}"
        );
        assert!(
            !cmd.contains(r"..\..\..\..\..\"),
            ".cmd shim should not climb above the `.aube/` root; got:\n{cmd}"
        );
    }

    /// Build a materialized dep that declares NO top-level `bin` but a
    /// `directories.bin` pointing at `mybins/`, containing executable
    /// files (incl. a nested subdir), and return the graph + project
    /// dir so a `link_bins` pass can be run end-to-end. This is the
    /// differential repro: npm 11 / pnpm 10 / yarn-classic / bun all
    /// link `directories.bin`; pre-fix aube linked nothing.
    fn fixture_dep_with_directories_bin(
        project_dir: &std::path::Path,
        manifest: &str,
    ) -> (LockfileGraph, PathBuf) {
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "dep-dirbin@1.0.0";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "dep-dirbin", 120, None);
        std::fs::create_dir_all(pkg_dir.join("mybins/nested")).unwrap();
        std::fs::write(pkg_dir.join("package.json"), manifest).unwrap();
        std::fs::write(pkg_dir.join("mybins/alpha"), "#!/usr/bin/env node\n").unwrap();
        std::fs::write(pkg_dir.join("mybins/beta"), "#!/usr/bin/env node\n").unwrap();
        // Recursive: a file in a subdir must also be linked (by basename).
        std::fs::write(pkg_dir.join("mybins/nested/gamma"), "#!/usr/bin/env node\n").unwrap();

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "dep-dirbin".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("dep-dirbin", "1.0.0", BTreeMap::new()),
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        (graph, aube_dir)
    }

    fn run_link_bins(
        project_dir: &std::path::Path,
        graph: &LockfileGraph,
        aube_dir: &std::path::Path,
    ) {
        link_bins(
            project_dir,
            "node_modules",
            aube_dir,
            graph,
            120,
            None,
            aube_linker::BinShimOptions::default(),
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
            &mut ManagedBinLinks::default(),
            None,
        )
        .unwrap();
    }

    /// THE PARITY REPRO. A dep with only `directories.bin` (no `bin`)
    /// must get every regular file under that dir linked into
    /// `.bin/<basename>`, recursively. Fails on pre-fix main (empty
    /// `.bin/`), passes after the fallback lands.
    #[test]
    fn links_directories_bin_when_top_level_bin_absent() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let (graph, aube_dir) = fixture_dep_with_directories_bin(
            project_dir,
            r#"{"name":"dep-dirbin","version":"1.0.0","directories":{"bin":"./mybins"}}"#,
        );
        run_link_bins(project_dir, &graph, &aube_dir);

        let bin = project_dir.join("node_modules/.bin");
        assert!(
            bin.join("alpha").exists(),
            "alpha (top-level file) must link"
        );
        assert!(bin.join("beta").exists(), "beta (top-level file) must link");
        assert!(
            bin.join("gamma").exists(),
            "gamma (nested file) must link recursively by basename"
        );
    }

    /// `bin` wins: when BOTH `bin` and `directories.bin` are present,
    /// only the `bin` field is honored (npm errors on both; we match
    /// pnpm's precedence — `bin` short-circuits `directories.bin`).
    #[test]
    fn top_level_bin_wins_over_directories_bin() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let (graph, aube_dir) = fixture_dep_with_directories_bin(
            project_dir,
            r#"{"name":"dep-dirbin","version":"1.0.0","bin":{"only-this":"mybins/alpha"},"directories":{"bin":"./mybins"}}"#,
        );
        run_link_bins(project_dir, &graph, &aube_dir);

        let bin = project_dir.join("node_modules/.bin");
        assert!(bin.join("only-this").exists(), "the `bin` entry must link");
        assert!(
            !bin.join("beta").exists() && !bin.join("gamma").exists(),
            "directories.bin must be ignored when `bin` is present"
        );
    }

    /// CONTAINMENT GUARD. A `directories.bin` that escapes the package
    /// root (`../escape`) must link nothing — the canonicalized bin dir
    /// is not a subdir of the package, so it is rejected wholesale.
    #[test]
    fn directories_bin_escaping_package_root_links_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "evil@1.0.0";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "evil", 120, None);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"evil","version":"1.0.0","directories":{"bin":"../escape"}}"#,
        )
        .unwrap();
        // A sibling `escape/` dir holding a file the attacker wants linked.
        // It lives OUTSIDE the package root (one level up).
        let escape = pkg_dir.parent().unwrap().join("escape");
        std::fs::create_dir_all(&escape).unwrap();
        std::fs::write(escape.join("pwned"), "#!/usr/bin/env node\n").unwrap();

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "evil".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("evil", "1.0.0", BTreeMap::new()),
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        run_link_bins(project_dir, &graph, &aube_dir);

        let bin = project_dir.join("node_modules/.bin");
        assert!(
            !bin.join("pwned").exists(),
            "a directories.bin that escapes the package root must link nothing"
        );
    }

    /// `directories.bin: "."` (the package root itself) links NOTHING,
    /// matching pnpm's `isSubdir`, which is false for equal paths. The
    /// reflexive `starts_with` would otherwise shim every file in the
    /// package (package.json, sources, …) into `.bin/`.
    #[test]
    fn directories_bin_pointing_at_package_root_links_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "rootdep@1.0.0";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "rootdep", 120, None);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"rootdep","version":"1.0.0","directories":{"bin":"."}}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("index.js"), "module.exports = {}\n").unwrap();

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "rootdep".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("rootdep", "1.0.0", BTreeMap::new()),
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        run_link_bins(project_dir, &graph, &aube_dir);

        let bin = project_dir.join("node_modules/.bin");
        assert!(
            !bin.join("index.js").exists() && !bin.join("package.json").exists(),
            "directories.bin pointing at the package root must link nothing"
        );
    }

    /// A symlinked subdirectory inside `directories.bin` is NOT
    /// followed, so files reached only via the symlink are not linked
    /// (pnpm: `followSymbolicLinks: false`). This stops a symlink from
    /// pulling the walk outside the package tree.
    #[cfg(unix)]
    #[test]
    fn directories_bin_does_not_follow_symlinked_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "symdep@1.0.0";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "symdep", 120, None);
        std::fs::create_dir_all(pkg_dir.join("mybins")).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"symdep","version":"1.0.0","directories":{"bin":"./mybins"}}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("mybins/real"), "#!/usr/bin/env node\n").unwrap();
        // Outside-the-package dir with a tempting file, reachable only
        // via a symlink planted inside mybins/.
        let outside = project_dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("escaped"), "#!/usr/bin/env node\n").unwrap();
        std::os::unix::fs::symlink(&outside, pkg_dir.join("mybins/link")).unwrap();

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "symdep".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("symdep", "1.0.0", BTreeMap::new()),
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        run_link_bins(project_dir, &graph, &aube_dir);

        let bin = project_dir.join("node_modules/.bin");
        assert!(bin.join("real").exists(), "the real in-tree file must link");
        assert!(
            !bin.join("escaped").exists(),
            "a file reachable only through a symlinked subdir must NOT link"
        );
    }

    /// Hoisted precedence. Both passes write `node_modules/.bin/probe`;
    /// the ORDER in `link_all_bins` is what resolves the collision, so a
    /// direct dep's bin must survive a hoisted transitive's. Names are
    /// chosen so dep_path order alone would pick the WRONG winner:
    /// `z-transitive` sorts last, so it wins inside the placements pass
    /// and only the later direct-dep pass can dislodge it. Reordering the
    /// two calls flips this assertion.
    ///
    /// Unix-only: it reads the winner back by canonicalizing the `.bin`
    /// entry, which requires the symlink layout. On Windows
    /// `create_bin_shim` writes `probe`/`probe.cmd`/`probe.ps1` as regular
    /// files, so `canonicalize` would resolve to the shim itself.
    #[cfg(unix)]
    #[test]
    fn hoisted_direct_dep_bin_beats_a_hoisted_transitive_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let modules = root.join("node_modules");

        let write_pkg = |name: &str| {
            let pkg_dir = modules.join(name);
            std::fs::create_dir_all(&pkg_dir).unwrap();
            std::fs::write(
                pkg_dir.join("package.json"),
                format!(r#"{{"name":"{name}","version":"1.0.0","bin":{{"probe":"cli.js"}}}}"#),
            )
            .unwrap();
            std::fs::write(pkg_dir.join("cli.js"), "#!/usr/bin/env node\n").unwrap();
        };
        write_pkg("a-direct");
        write_pkg("z-transitive");

        let bin_with_probe = || {
            let mut b = BTreeMap::new();
            b.insert("probe".to_string(), "cli.js".to_string());
            b
        };
        let mut direct = locked("a-direct", "1.0.0", bin_with_probe());
        direct
            .dependencies
            .insert("z-transitive".to_string(), "1.0.0".to_string());

        let mut packages = BTreeMap::new();
        packages.insert("a-direct@1.0.0".to_string(), direct);
        packages.insert(
            "z-transitive@1.0.0".to_string(),
            locked("z-transitive", "1.0.0", bin_with_probe()),
        );
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "a-direct".to_string(),
                dep_path: "a-direct@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            }],
        );
        let graph = LockfileGraph {
            packages,
            importers,
            ..Default::default()
        };

        let placements = aube_linker::HoistedPlacements::from_graph(
            root,
            &graph,
            "node_modules",
            aube_linker::HoistingLimits::None,
        )
        .unwrap();
        assert!(
            placements.package_dir("z-transitive@1.0.0").is_some(),
            "fixture is inert unless the transitive was actually placed"
        );

        let aube_dir = modules.join(".aube");
        let shim_opts = aube_linker::BinShimOptions::default();
        let mut cache = PkgJsonCache::new();
        // Same order as `link_all_bins`: placements, then direct deps.
        let mut managed = ManagedBinLinks::default();
        link_hoisted_placement_bins(
            &aube_dir,
            &graph,
            120,
            &placements,
            shim_opts,
            &mut cache,
            &mut managed,
            None,
        )
        .unwrap();
        assert!(
            modules.join(".bin/probe").symlink_metadata().is_ok(),
            "the hoisted transitive's bin must be linked at all (the bug)"
        );
        link_bins(
            root,
            "node_modules",
            &aube_dir,
            &graph,
            120,
            Some(&placements),
            shim_opts,
            &mut cache,
            None,
            &mut WsPkgJsonCache::new(),
            &mut managed,
            None,
        )
        .unwrap();

        // Canonicalize BOTH sides: on macOS a tempdir lives under `/var`, a
        // symlink to `/private/var`, so resolving only one side fails the
        // prefix check on the path spelling rather than on the behavior.
        let resolved = std::fs::canonicalize(modules.join(".bin/probe")).unwrap();
        let expected_owner = std::fs::canonicalize(modules.join("a-direct")).unwrap();
        assert!(
            resolved.starts_with(&expected_owner),
            "direct dep must win the name collision; `probe` resolved to {}, expected a target under {}",
            resolved.display(),
            expected_owner.display()
        );
    }
}
