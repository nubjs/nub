use aube_lockfile::dep_path_filename::dep_path_to_filename;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) type PkgJsonCache = BTreeMap<String, Option<serde_json::Value>>;

/// Per-install cache of workspace-package `package.json` reads. Keyed
/// by the workspace dir on disk so a popular tooling package consumed
/// by many importers gets read and parsed once, not once per consumer.
pub(crate) type WsPkgJsonCache = BTreeMap<PathBuf, Option<serde_json::Value>>;

/// Link bin entries from packages to node_modules/.bin/
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
pub(super) fn dep_modules_dir_for(package_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
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
    pkg_dir: &std::path::Path,
    name: &str,
) -> miette::Result<Option<serde_json::Value>> {
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

fn read_materialized_pkg_json_cached(
    cache: &mut PkgJsonCache,
    dep_path: &str,
    pkg_dir: &std::path::Path,
    name: &str,
) -> miette::Result<Option<serde_json::Value>> {
    if let Some(value) = cache.get(dep_path) {
        return Ok(value.clone());
    }
    let value = read_materialized_pkg_json(pkg_dir, name)?;
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
) -> miette::Result<()> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    link_bins_for_dep_at(cache, bin_dir, graph, dep_path, name, &pkg_dir, shim_opts)
}

/// [`link_bins_for_dep`] with the dep's materialized directory supplied by the
/// caller, resolving every target against that `pkg_dir`.
///
/// Two callers need to name the directory themselves. `link_dep_bins` must name
/// it in the virtual store's own coordinate system rather than re-deriving it
/// from `aube_dir` — see the store-resolution note there. The hoisted pass needs
/// the *concrete* placement: a package whose name conflicts with a shallower
/// version is materialized once per site and each site's shims must point at its
/// own copy, while `materialized_pkg_dir` only ever returns the shallowest.
pub(super) fn link_bins_for_dep_at(
    cache: &mut PkgJsonCache,
    bin_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    name: &str,
    pkg_dir: &std::path::Path,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    if let Some(pkg_json) = read_materialized_pkg_json_cached(cache, dep_path, pkg_dir, name)? {
        if let Some(bin) = pkg_json.get("bin") {
            link_bin_entries(bin_dir, pkg_dir, Some(name), bin, shim_opts)?;
        } else if let Some(dir_bin) = pkg_json.get("directories").and_then(|d| d.get("bin")) {
            // `bin` wins; `directories.bin` is the fallback only.
            link_dir_bins(bin_dir, pkg_dir, dir_bin, shim_opts)?;
        }
    }
    link_bundled_bins(bin_dir, pkg_dir, graph, dep_path, shim_opts)?;
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
fn link_hoisted_placement_bins(
    graph: &aube_lockfile::LockfileGraph,
    placements: &aube_linker::HoistedPlacements,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
) -> miette::Result<()> {
    for (dep_path, pkg_dir) in placements.iter() {
        let Some(pkg) = graph.get_package(dep_path) else {
            continue;
        };
        let bin_dir = dep_modules_dir_for(pkg_dir, &pkg.name).join(".bin");
        link_bins_for_dep_at(
            cache, &bin_dir, graph, dep_path, &pkg.name, pkg_dir, shim_opts,
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
) -> miette::Result<()> {
    let bin_dir = project_dir.join(modules_dir_name).join(".bin");
    std::fs::create_dir_all(&bin_dir).into_diagnostic()?;

    for dep in graph.root_deps() {
        if let Some(ws_dir) = ws_dirs.and_then(|m| m.get(&dep.name)) {
            link_bins_for_workspace_dep(ws_cache, &bin_dir, ws_dir, &dep.name, shim_opts)?;
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
            )?;
        }
    }

    Ok(())
}

/// Everything the link phase needs to lay down `.bin/` shims: the root
/// project's direct-dep bins, the root/workspace-member self-bins, each
/// workspace importer's dep bins, and the per-dep `.bin/` for transitive
/// build-script PATH. Shared by `run_link_phase` (the initial pass) and
/// `run_finalize_phase` (the re-link after dep build scripts run — a
/// script can replace a JS launcher with a native binary, e.g. esbuild
/// #394, and the shim must be regenerated against the post-build target).
pub(crate) struct LinkAllBinsInput<'a> {
    pub(crate) settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub(crate) node_linker: aube_linker::NodeLinker,
    pub(crate) cwd: &'a Path,
    pub(crate) modules_dir_name: &'a str,
    pub(crate) aube_dir: &'a Path,
    pub(crate) graph_for_link: &'a aube_lockfile::LockfileGraph,
    pub(crate) virtual_store_dir_max_length: usize,
    pub(crate) placements: Option<&'a aube_linker::HoistedPlacements>,
    pub(crate) manifest: &'a aube_manifest::PackageJson,
    pub(crate) manifests: &'a [(String, aube_manifest::PackageJson)],
    pub(crate) ws_dirs: &'a BTreeMap<String, PathBuf>,
    pub(crate) has_workspace: bool,
    pub(crate) virtual_store_only: bool,
    pub(crate) ignore_scripts: bool,
    pub(crate) has_any_allow_rule: bool,
    pub(crate) floor_may_allow_any: bool,
}

/// Derive the shim layout from settings the same way `run_link_phase`
/// does, then lay down every `.bin/` shim. See [`LinkAllBinsInput`].
pub(crate) fn link_all_bins(input: LinkAllBinsInput<'_>) -> miette::Result<()> {
    let LinkAllBinsInput {
        settings_ctx,
        node_linker,
        cwd,
        modules_dir_name,
        aube_dir,
        graph_for_link,
        virtual_store_dir_max_length,
        placements,
        manifest,
        manifests,
        ws_dirs,
        has_workspace,
        virtual_store_only,
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
    } = input;

    if virtual_store_only {
        return Ok(());
    }

    // `extendNodePath` controls whether shim scripts export `NODE_PATH`.
    // `preferSymlinkedExecutables` only matters on POSIX: `Some(true)`
    // keeps the symlink layout, `Some(false)` swaps in a shell shim so
    // `extendNodePath` can actually take effect (bare symlinks can't set
    // env vars). When the user leaves it unset, default to shim under the
    // isolated linker (NODE_PATH matters there so transitives hoisted to
    // `.aube/node_modules/` resolve from a shimmed bin) and symlink under
    // hoisted. Mirrors pnpm's effective default. Windows always writes
    // cmd/ps1/sh wrappers regardless. (A native-executable target always
    // bypasses the shim regardless of this setting — see `create_bin_shim`.)
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
    let ws_dirs_for_bins = has_workspace.then_some(ws_dirs);
    // Writers into a SHARED `.bin` run lowest-precedence first, because
    // every later pass overwrites a same-named shim (`create_bin_shim`
    // unlinks before it writes). That makes the order below the whole
    // conflict resolution for the hoisted layout:
    //   hoisted placements < direct deps < self-bin.
    // `maybe_link_dep_bins` is deliberately NOT part of this sequence — it
    // is isolated-only, and its per-dep targets are disjoint from every
    // `.bin` written here, so it stays at the end where standalone callers
    // (`rebuild`) can reuse it without inheriting an ordering contract.
    if let Some(placements) = placements {
        link_hoisted_placement_bins(graph_for_link, placements, shim_opts, &mut pkg_json_cache)?;
    }
    link_bins(
        cwd,
        modules_dir_name,
        aube_dir,
        graph_for_link,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        &mut pkg_json_cache,
        ws_dirs_for_bins,
        &mut ws_pkg_json_cache,
    )?;
    // Root importer's own `bin` (discussion #228). Runs after `link_bins`
    // so a self-bin overrides a same-named dep bin. Self-bin targets are
    // files in the importer's own tree — often build outputs that don't
    // exist at install time, or are later restored from an
    // `actions/upload-artifact` round-trip that strips the POSIX exec bit.
    // A POSIX shim (shell script that invokes `node`) is itself `+x` and
    // does not rely on the target's exec bit, so `aube run` works in both
    // flows.
    if let Some(bin) = manifest.extra.get("bin") {
        let root_bin_dir = cwd.join(modules_dir_name).join(".bin");
        let self_shim_opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..shim_opts
        };
        link_bin_entries(
            &root_bin_dir,
            cwd,
            manifest.name.as_deref(),
            bin,
            self_shim_opts,
        )?;
    }
    if has_workspace {
        for (importer_path, deps) in &graph_for_link.importers {
            if importer_path == "." {
                continue;
            }
            // pnpm v9 emits nested peer-context importer entries (e.g.
            // `a/node_modules/@scope/b`). Those paths are reached through
            // the workspace-to-workspace symlink chain, not distinct
            // directories to receive their own `.bin`. Walking them here
            // duplicates work on the physical workspace and, at monorepo
            // depth, pushes the kernel's per-lookup symlink budget over
            // SYMLOOP_MAX.
            if !aube_linker::is_physical_importer(importer_path) {
                continue;
            }
            let pkg_dir = cwd.join(importer_path);
            let bin_dir = pkg_dir.join(modules_dir_name).join(".bin");
            std::fs::create_dir_all(&bin_dir).into_diagnostic()?;
            for dep in deps {
                if let Some(ws_dir) = ws_dirs.get(&dep.name) {
                    link_bins_for_workspace_dep(
                        &mut ws_pkg_json_cache,
                        &bin_dir,
                        ws_dir,
                        &dep.name,
                        shim_opts,
                    )?;
                } else {
                    link_bins_for_dep(
                        &mut pkg_json_cache,
                        aube_dir,
                        &bin_dir,
                        graph_for_link,
                        &dep.dep_path,
                        &dep.name,
                        virtual_store_dir_max_length,
                        placements,
                        shim_opts,
                    )?;
                }
            }
            // Workspace member's own `bin` (discussion #228). `manifests`
            // was parsed once upstream and keys by importer relpath. See
            // the root self-bin call site for why this forces a POSIX shim.
            if let Some((_, member_manifest)) = manifests.iter().find(|(p, _)| p == importer_path)
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
                )?;
            }
        }
    }
    // Gate matches the lifecycle phase's (`finalize.rs`) via the shared
    // `dep_build_scripts_may_run` predicate, threaded through
    // `maybe_link_dep_bins`: the `defaultTrust` floor can authorize a
    // package's build scripts with no explicit allow rule, and those
    // scripts call binaries declared in the package's own `dependencies`
    // — which must be shimmed into the dep's `.bin` and put on PATH.
    maybe_link_dep_bins(
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
        aube_dir,
        graph_for_link,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        &mut pkg_json_cache,
    )?;
    Ok(())
}

/// Link bins declared by a `workspace:` dep into the importer's
/// `.bin/`. Workspace deps don't get a `.aube/<dep_path>/` materialization
/// (the linker symlinks them straight into the importer's `node_modules/`),
/// so `link_bins_for_dep` finds nothing on disk and silently skips. Read
/// the workspace package's own `package.json` and shim each bin entry,
/// matching pnpm's behavior of exposing workspace bins to dependent
/// packages' npm scripts.
///
/// `cache` deduplicates the read+parse across importers — without it,
/// a popular tooling package consumed by N workspace members gets its
/// `package.json` read N times during a single install.
pub(super) fn link_bins_for_workspace_dep(
    cache: &mut WsPkgJsonCache,
    bin_dir: &Path,
    ws_dir: &Path,
    name: &str,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    let pkg_json = if let Some(cached) = cache.get(ws_dir) {
        cached.clone()
    } else {
        let pkg_json_path = ws_dir.join("package.json");
        let parsed = match std::fs::read_to_string(&pkg_json_path) {
            Ok(content) => Some(
                aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
                    .map_err(miette::Report::new)
                    .wrap_err_with(|| {
                        format!("failed to parse package.json for workspace dep {name}")
                    })?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for workspace dep {name} at {}: {e}",
                    pkg_json_path.display()
                ));
            }
        };
        cache.insert(ws_dir.to_path_buf(), parsed.clone());
        parsed
    };
    if let Some(pkg_json) = pkg_json {
        if let Some(bin) = pkg_json.get("bin") {
            link_bin_entries(bin_dir, ws_dir, Some(name), bin, shim_opts)?;
        } else if let Some(dir_bin) = pkg_json.get("directories").and_then(|d| d.get("bin")) {
            link_dir_bins(bin_dir, ws_dir, dir_bin, shim_opts)?;
        }
    }
    Ok(())
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
) -> miette::Result<()> {
    if !super::default_trust::dep_build_scripts_may_run(
        ignore_scripts,
        has_any_allow_rule,
        floor_may_allow_any,
    ) {
        return Ok(());
    }
    link_dep_bins(
        aube_dir,
        graph,
        virtual_store_dir_max_length,
        placements,
        shim_opts,
        cache,
    )
}

/// Write per-dep `.bin/` directories holding shims for each package's
/// *own* declared dependencies, and — first, so the children win a name
/// they share — the package's own `bin`. The children half mirrors pnpm's
/// post-link pass that populates
/// `node_modules/.pnpm/<dep_path>/node_modules/.bin/`; the self-bin half is
/// a DELIBERATE divergence from pnpm toward npm, and [`link_own_bins`]
/// carries the reasoning.
///
/// Without the children half, a dep's lifecycle script (e.g.
/// `unrs-resolver`'s postinstall that calls `prebuild-install`) can't find
/// transitive binaries on PATH — the project-level `node_modules/.bin` only
/// holds shims for the root's *direct* deps. `run_dep_hook` walks the
/// enclosing `.bin` chain closest-first, so the dep's own transitive bins
/// win.
///
/// Isolated mode only. Under hoisted, `link_hoisted_placement_bins` already
/// puts every placed package's bins in the `.bin` beside it, which is where
/// Node resolves that copy from, and `run_dep_hook`'s chain walk reaches
/// every one of those directories. Running this pass under hoisted would
/// instead write a nested child's bins into the *enclosing* (often root)
/// `.bin` — a shared directory whose contents are decided by pass order
/// inside `link_all_bins`, which standalone callers like `rebuild` do not
/// reproduce. Skipping keeps that directory owned by the ordered passes.
pub(crate) fn link_dep_bins(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
) -> miette::Result<()> {
    if placements.is_some() {
        // Hoisted — skip. See function doc.
        return Ok(());
    }
    for (dep_path, pkg) in &graph.packages {
        // No `dependencies.is_empty()` fast path: a package with no deps at
        // all still gets the self-bin pass below. The package.json read it
        // adds is keyed by dep_path in the shared cache, which every other
        // pass reads through too, so the graph is still parsed once.
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
        // Under the GVS, `.aube/<dep_path>` is a symlink into the shared store,
        // so this `.bin/` has TWO legitimate spellings — the project's
        // `<dep_path>` and the store's `<dep_path>-<graph-hash>` — and a shim's
        // `basedir=$(dirname "$0")` is lexical over whichever one its invoker
        // used. Both occur in production: the lifecycle runner puts the project
        // spelling on PATH, and nub's macOS build jail canonicalizes the child's
        // PATH (Seatbelt matches resolved paths, and an ungranted symlinked entry
        // fails `posix_spawnp` with a FATAL EPERM that masks every later entry),
        // handing the same shim a store-spelled `$0`. So the emitted target must
        // not depend on which spelling reached it — see the child loop below.
        let dep_modules_dir = dep_modules_dir_for(&pkg_dir, &pkg.name);
        let bin_dir = dep_modules_dir.join(".bin");
        // Don't `create_dir_all(&bin_dir)` here — most deps have
        // no child that ships a `bin`, and an eager mkdir would leave
        // empty `.bin/` directories everywhere. `create_bin_link`
        // materializes the parent the first time a shim actually
        // lands, so deps whose children contribute zero shims stay
        // empty on disk.

        // BEFORE the child loop, so a child declaring the same bin name is
        // written second and wins it — see `link_own_bins`.
        link_own_bins(cache, &bin_dir, dep_path, &pkg.name, &pkg_dir, shim_opts)?;

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
            // Reach the child through the SIBLING SYMLINK the linker wrote next
            // to this `.bin/`, not through a path re-derived from `aube_dir`.
            // That keeps the emitted relative target inside the dep's own
            // `node_modules/` (`../<child_name>/…`), so it never names the
            // virtual-store entry whose leaf differs between the two spellings
            // above — the symlink itself absorbs the difference, because the
            // kernel walks it from its physical parent. A re-derived path
            // escapes to the store root and is correct in one spelling only.
            // It also tracks the linker exactly: the sibling is written under
            // the EDGE name, so an aliased dep (`"x": "npm:y@1"`) resolves here
            // the same way `require('x')` does from inside the package.
            // The sibling may have been filtered (optional on another
            // platform); `link_bins_for_dep_at` already returns Ok when
            // the target pkg_json is absent, so just call through.
            let child_pkg_dir = dep_modules_dir.join(child_name);
            link_bins_for_dep_at(
                cache,
                &bin_dir,
                graph,
                &child_dep_path,
                child_name,
                &child_pkg_dir,
                shim_opts,
            )?;
        }
    }
    Ok(())
}

/// Shim a package's OWN `bin` into the `.bin` beside it in the virtual
/// store, before the children that share that directory are written.
///
/// ⛔ A PACKAGE'S OWN BIN IS REACHABLE FROM ITS OWN LIFECYCLE SCRIPT UNDER npm,
/// AND WAS NOT HERE.
///
/// npm's flat layout puts a top-level dependency's bin in
/// `<project>/node_modules/.bin`, which is the SAME `node_modules` the
/// package sits in. Scripts exploit that: they derive `.bin` from
/// `__dirname` and expect their own entry to be there. Under any isolated
/// layout `__dirname` realpaths into the store cell, so the derived
/// directory is `<cell>/node_modules/.bin` — which held only this package's
/// CHILDREN's bins.
///
/// MEASURED on `@typescript-tools/rust-implementation@7.0.8`, whose
/// `postinstall` (`node npm/install.js`) downloads a native binary and then
/// calls `fs.unlinkSync(<derived .bin>/monorepo)` with no `existsSync` guard,
/// meaning to replace its own dummy launcher:
///   npm  writes node_modules/.bin/monorepo               -> unlink succeeds
///   aube wrote .store/<cell>/node_modules/.bin/{rimraf}  -> ENOENT, rc=1
/// Reproduces with `install.buildJail=false`, so it is a linker gap and not
/// confinement. Real pnpm 10.15.1 fails identically — this pass is where nub
/// deliberately follows npm instead. Same shape and same additive framing as
/// the bundled-dep hoist in `link_bundled_bins` above.
///
/// ⛔ RUNS BEFORE THE CHILD LOOP, AND THAT ORDER IS THE WHOLE SAFETY ARGUMENT.
/// It is the idiom `link_all_bins` already documents — pass order IS the
/// conflict table, because `create_bin_shim` unlinks before it writes. A child
/// declaring the same bin name is written second and wins it, so every name
/// that resolves today keeps resolving to exactly what it does today and this
/// pass can only ADD a name that resolved nowhere. npm parity on an already
/// ambiguous name is not worth changing a name that already works.
///
/// A skip-if-the-name-is-taken rule was tried first and is WRONG, measured:
/// `link_all_bins` runs a SECOND time after dep lifecycle scripts, to
/// re-classify a bin a build replaced (esbuild's JS launcher becoming a native
/// binary, #394). Skipping an occupied name makes that relink decline to
/// refresh this pass's OWN earlier write, so `esbuild@0.25.12` kept a stale
/// `node <native-binary>` wrapper in its cell `.bin` and died `SyntaxError:
/// Invalid or unexpected token`. Overwriting on every pass is what keeps the
/// relink idempotent, which is what it was built to be.
fn link_own_bins(
    cache: &mut PkgJsonCache,
    bin_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    pkg_dir: &std::path::Path,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    let Some(pkg_json) = read_materialized_pkg_json_cached(cache, dep_path, pkg_dir, name)? else {
        return Ok(());
    };
    if let Some(bin) = pkg_json.get("bin") {
        link_bin_entries(bin_dir, pkg_dir, Some(name), bin, shim_opts)?;
    } else if let Some(dir_bin) = pkg_json.get("directories").and_then(|d| d.get("bin")) {
        // `bin` wins; `directories.bin` is the fallback only. Same rule the
        // dep pass applies in `link_bins_for_dep_at`.
        link_dir_bins(bin_dir, pkg_dir, dir_bin, shim_opts)?;
    }
    Ok(())
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
) -> miette::Result<()> {
    let Some(locked) = graph.get_package(dep_path) else {
        return Ok(());
    };
    // ⛔ A BUNDLED DEP'S BIN IS NEEDED INSIDE THE BUNDLING PACKAGE, NOT ONLY IN THE CONSUMER'S.
    //
    // This linked into `bin_dir` alone — the `.bin` of whoever depends on this package. That is the
    // wrong scope for the commonest use of a bundled dep: the bundling package's OWN lifecycle
    // script invoking it. Such a script runs with cwd = the bundling package's directory, so PATH
    // resolution walks up from there and never reaches the consumer's `.bin`.
    //
    // MEASURED on `@pulumi/kubernetes@0.21.1`, and it is why the install fails outright:
    //   grpc@1.21.1 bundles node-pre-gyp and its install script is
    //   `node-pre-gyp install --fallback-to-build`.
    //   nub wrote  .store/@pulumi+pulumi@0.17.28/node_modules/.bin/node-pre-gyp   (the CONSUMER)
    //   npm writes node_modules/grpc/node_modules/.bin/node-pre-gyp               (the BUNDLER)
    //   -> `node-pre-gyp: not found`, rc=1. Reproduces with install.buildJail=false, so it is a
    //   linker defect and not confinement — it was surfacing in the corpus as a jail failure.
    //
    // Additive on purpose. The consumer-scoped write stays: it is what the root-importer and
    // workspace callers documented above rely on, and removing it would be a separate behavioural
    // change with its own blast radius. A bundled dep is private to its bundler, so the new write
    // can only ever ADD a name inside that package's own tree.
    for bundled in &locked.bundled_dependencies {
        let bundled_dir = pkg_dir.join("node_modules").join(bundled);
        let bundled_pkg_json_path = bundled_dir.join("package.json");
        let Ok(content) = std::fs::read_to_string(&bundled_pkg_json_path) else {
            continue;
        };
        let Ok(bundled_pkg_json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let own_bin_dir = pkg_dir.join("node_modules").join(".bin");
        for target_bin_dir in [bin_dir, own_bin_dir.as_path()] {
            if let Some(bin) = bundled_pkg_json.get("bin") {
                link_bin_entries(target_bin_dir, &bundled_dir, Some(bundled), bin, shim_opts)?;
            } else if let Some(dir_bin) = bundled_pkg_json
                .get("directories")
                .and_then(|d| d.get("bin"))
            {
                link_dir_bins(target_bin_dir, &bundled_dir, dir_bin, shim_opts)?;
            }
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
                create_bin_link(bin_dir, bin_name, &pkg_dir.join(bin_path), shim_opts)?;
            }
        }
        serde_json::Value::Object(bins) => {
            for (bin_name, path) in bins {
                if let Some(path_str) = path.as_str()
                    && aube_linker::validate_bin_name(bin_name).is_ok()
                    && aube_linker::validate_bin_target(path_str).is_ok()
                {
                    create_bin_link(bin_dir, bin_name, &pkg_dir.join(path_str), shim_opts)?;
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
        create_bin_link(bin_dir, file_name, file, shim_opts)?;
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
) -> miette::Result<()> {
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
        let child_name = "node-gyp-build-optional-packages";

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
        // Runnable, and runnable BY NODE specifically: `detect_interpreter`
        // reads this shebang into the shim, and node is the only interpreter
        // that collapses the target's `..` segments lexically rather than
        // walking them through the store symlink. A shim that can be executed
        // is what lets the store-key test assert on a real invocation.
        std::fs::write(
            child_dir.join("bin/build.js"),
            "#!/usr/bin/env node\nconsole.log('ngb-ok')\n",
        )
        .unwrap();

        // The sibling symlink `materialize_into` writes next to the parent's
        // own directory. Not decoration: it is what `require('<child>')`
        // resolves through from inside the parent, and now what the per-dep
        // `.bin` shim's target is expressed against — a fixture without it
        // would let a target that can only resolve via `aube_dir` pass.
        let sibling = dep_modules_dir_for(&parent_dir, "lmdb").join(child_name);
        // Relative on POSIX (survives the store relocation the GVS test
        // performs), absolute on Windows where `create_dir_link` writes a
        // junction — the same split `materialize_into` makes.
        #[cfg(not(windows))]
        let sibling_target = std::path::PathBuf::from("..")
            .join("..")
            .join(dep_path_to_filename(child_dep_path, 120))
            .join("node_modules")
            .join(child_name);
        #[cfg(windows)]
        let sibling_target = child_dir.clone();
        aube_linker::create_dir_link(&sibling_target, &sibling).unwrap();

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

    /// THE STORE-KEY REGRESSION. Under the global virtual store each
    /// `.aube/<dep_path>` is a symlink into a machine-global store whose
    /// entries carry a graph-hash suffix, so a per-dep `.bin/` derived from
    /// the project side lands PHYSICALLY in that store. The shim is a
    /// `/bin/sh` script whose `basedir=$(dirname "$0")` is purely LEXICAL, so
    /// what it resolves against is the path its INVOKER used — and the entry
    /// has two spellings that differ in exactly the leaf a re-derived target
    /// names. Whichever one such a target picks, the other invocation lands on
    /// a directory that does not exist, and every transitive CLI a build script
    /// shells out to (`node-gyp-build`, `prebuild-install`, node-pre-gyp — the
    /// whole native-addon family) dies with `Cannot find module`.
    ///
    /// BOTH spellings are production geometries, which is why the test RUNS the
    /// shim from each rather than inspecting the target's text. The lifecycle
    /// runner puts the project spelling on PATH; nub's macOS build jail then
    /// canonicalizes that PATH before spawning (Seatbelt matches resolved paths,
    /// and one ungranted symlinked entry fails `posix_spawnp` with a FATAL
    /// EPERM that masks every later entry), so the same shim is reached through
    /// the store spelling with a store-rooted `$0`. Measured: 0 store-rooted
    /// invocations on Linux, where that backend canonicalizes only the child's
    /// cwd — the divergence is the backend, not the corpus.
    ///
    /// Each half is the other's control, so neither can pass hollowly: a target
    /// re-derived from the project's `.aube` fails the store-side run (the
    /// reported `Cannot find module`), and one re-derived from the store fails
    /// the project-side run. Only a target that stays inside the dep's own
    /// `node_modules/` — reached through the sibling symlink, which the kernel
    /// walks from its physical parent either way — satisfies both.
    ///
    /// Node is load-bearing as the interpreter, not incidental: it collapses the
    /// target's `..` with `path.resolve` before opening it, so the walk stays in
    /// whichever coordinate system `$0` supplied.
    #[test]
    #[cfg(unix)]
    fn dep_bin_shims_resolve_when_the_entry_is_a_symlink_into_a_hashed_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let aube_dir = dir.path().join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).unwrap();

        // Build the graph + package trees under a throwaway `.aube`, then move
        // each entry into the store under its hashed name and leave a symlink
        // behind — the exact shape `link_all`'s GVS step 1 produces.
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let (graph, _) = fixture_parent_with_bin_bearing_child(&staging);
        for (dep_path, hash) in [
            ("lmdb@3.0.0", "0123456789abcdef"),
            ("node-gyp-build-optional-packages@5.2.0", "fedcba9876543210"),
        ] {
            let entry = dep_path_to_filename(dep_path, 120);
            let hashed = store.join(format!("{entry}-{hash}"));
            std::fs::create_dir_all(store.join("node_modules")).unwrap();
            std::fs::rename(staging.join(&entry), &hashed).unwrap();
            std::os::unix::fs::symlink(&hashed, aube_dir.join(&entry)).unwrap();
        }
        // `materialize_into` spells a cell's sibling symlinks with the HASHED
        // sibling name when it materializes into the store (`apply_hashes`), so
        // they resolve from the cell's physical parent. The staging fixture
        // wrote the project spelling; the relocation above left it dangling.
        let sibling = store
            .join("lmdb@3.0.0-0123456789abcdef")
            .join("node_modules")
            .join("node-gyp-build-optional-packages");
        std::fs::remove_file(&sibling).unwrap();
        std::os::unix::fs::symlink(
            "../../node-gyp-build-optional-packages@5.2.0-fedcba9876543210/node_modules/\
             node-gyp-build-optional-packages",
            &sibling,
        )
        .unwrap();

        let hidden = aube_dir.join("node_modules");
        link_dep_bins(
            &aube_dir,
            &graph,
            120,
            None,
            aube_linker::BinShimOptions {
                extend_node_path: true,
                // The isolated linker's shape: a shell shim carrying a RELATIVE
                // target, which is the form the store-key mismatch broke.
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: Some(&hidden),
            },
            &mut PkgJsonCache::new(),
        )
        .unwrap();

        // RUN the shim, once per spelling. Executing rather than stat-ing the
        // resolved target is what makes this non-tautological: `$basedir` is
        // `dirname "$0"`, so only a real invocation expands it in the coordinate
        // system a consumer supplied.
        let project_shim = aube_dir
            .join("lmdb@3.0.0")
            .join("node_modules/.bin/node-gyp-build-optional-packages");
        let body = std::fs::read_to_string(&project_shim)
            .unwrap_or_else(|e| panic!("no shim at {}: {e}", project_shim.display()));
        // The two namespaces have to genuinely diverge or the runs below prove
        // nothing: the shim FILE is physically in the hashed store, reachable
        // from the project path only through the `.aube` symlink.
        let store_shim = std::fs::canonicalize(&project_shim).unwrap();
        assert!(
            store_shim.starts_with(std::fs::canonicalize(&store).unwrap()),
            "fixture must leave the shim physically in the hashed store, else \
             the invocations below cannot tell the two namespaces apart; got {}",
            store_shim.display()
        );
        for (spelling, shim) in [("project", &project_shim), ("store", &store_shim)] {
            let out = std::process::Command::new(shim)
                .output()
                .unwrap_or_else(|e| panic!("could not spawn {}: {e}", shim.display()));
            assert!(
                out.status.success() && String::from_utf8_lossy(&out.stdout).contains("ngb-ok"),
                "shim must run when invoked through its {spelling} spelling; \
                 status {:?}\nstdout: {}\nstderr: {}\nshim:\n{body}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        let target =
            aube_linker::parse_posix_shim_target(&body).expect("shim must carry its target marker");

        // Every path a store-resident shim embeds is read by all sharers, so
        // none may be absolute. Pin the emitted values rather than merely
        // asserting one project's prefix is absent — a relative target is the
        // property that actually makes the file shareable.
        assert!(
            !target.starts_with('/'),
            "shim target must be relative to be shareable, got {target}"
        );
        // The invariant the two runs above demonstrate, stated directly: a
        // target confined to the dep's own `node_modules/` (one `..`, out of
        // `.bin/`) never names a virtual-store entry, so it cannot depend on
        // which spelling of that entry `$0` carried.
        assert_eq!(
            target, "../node-gyp-build-optional-packages/bin/build.js",
            "shim target must stay inside the dep's own node_modules"
        );
        assert!(
            body.contains(r#"export NODE_PATH="$basedir/..:$basedir/../../../node_modules""#),
            "NODE_PATH must stay relative to $basedir:\n{body}"
        );
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
        )
        .unwrap();
        assert!(
            !expected_shim2.exists(),
            "with no allow rule and the floor closed, no scripts run, so the \
             dep-bin pass must be skipped (fast path) — no shim should appear"
        );
    }

    /// Materialize one package at `<aube_dir>/<escaped dep_path>/node_modules/
    /// <name>` with a `package.json` declaring `bin`, and create each target
    /// file. Returns the package dir.
    fn materialize_pkg_with_bins(
        aube_dir: &std::path::Path,
        dep_path: &str,
        name: &str,
        bins: &[(&str, &str)],
    ) -> std::path::PathBuf {
        let pkg_dir = materialized_pkg_dir(aube_dir, dep_path, name, 120, None);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let bin_json: BTreeMap<&str, &str> = bins.iter().copied().collect();
        std::fs::write(
            pkg_dir.join("package.json"),
            serde_json::json!({ "name": name, "bin": bin_json }).to_string(),
        )
        .unwrap();
        for (_, target) in bins {
            let path = pkg_dir.join(target);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/usr/bin/env node\n").unwrap();
        }
        pkg_dir
    }

    /// The sibling symlink `materialize_into` writes next to a package's own
    /// directory. `link_dep_bins` reaches every child through it rather than
    /// re-deriving a path from `aube_dir`, so a fixture without it links
    /// nothing.
    fn link_child_sibling(
        aube_dir: &std::path::Path,
        parent_dir: &std::path::Path,
        parent_name: &str,
        child_dep_path: &str,
        child_name: &str,
    ) {
        let sibling = dep_modules_dir_for(parent_dir, parent_name).join(child_name);
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        // Relative on POSIX, absolute on Windows where `create_dir_link` writes
        // a junction — the same split `materialize_into` makes.
        #[cfg(not(windows))]
        let target = {
            let _ = aube_dir;
            std::path::PathBuf::from("..")
                .join("..")
                .join(dep_path_to_filename(child_dep_path, 120))
                .join("node_modules")
                .join(child_name)
        };
        #[cfg(windows)]
        let target = materialized_pkg_dir(aube_dir, child_dep_path, child_name, 120, None);
        aube_linker::create_dir_link(&target, &sibling).unwrap();
    }

    /// A package's OWN bin must be reachable from the `.bin` its OWN lifecycle
    /// script derives from `__dirname`, because npm's flat layout puts it there
    /// and real postinstalls depend on that. Measured on
    /// `@typescript-tools/rust-implementation@7.0.8`, whose postinstall
    /// `fs.unlinkSync`es its own `.bin` entry with no `existsSync` guard and
    /// died ENOENT.
    ///
    /// The pass is strictly ADDITIVE: a child declaring the same bin name is
    /// written after it and wins, so it can only fill a name that resolved
    /// nowhere. And because `link_all_bins` runs a SECOND time after dep
    /// lifecycle scripts, a self-bin the build replaced must be re-classified
    /// on that pass rather than left stale. All four contracts ride one graph
    /// because they are one pass over it.
    #[test]
    fn link_dep_bins_adds_a_package_own_bin_without_displacing_a_child_bin() {
        let dir = tempfile::tempdir().unwrap();
        let aube_dir = dir.path().join("node_modules/.store");

        // (a) The reported shape: a SCOPED package with its own bin, plus a
        //     child contributing a different name to the same `.bin`.
        let tool_dir = materialize_pkg_with_bins(
            &aube_dir,
            "@scope/tool@1.0.0",
            "@scope/tool",
            // Extensionless target, like the real cases (`bin/monorepo`,
            // `bin/esbuild`): `default_launch_for_target` lets a known script
            // EXTENSION pin the interpreter, so only an extensionless bin can
            // be re-classified as native after a build replaces it.
            &[("tool-cli", "bin/tool")],
        );
        materialize_pkg_with_bins(&aube_dir, "helper@1.0.0", "helper", &[("helper", "h.js")]);
        link_child_sibling(
            &aube_dir,
            &tool_dir,
            "@scope/tool",
            "helper@1.0.0",
            "helper",
        );

        // (b) Collision: the package's own bin name is already a child's.
        let host_dir =
            materialize_pkg_with_bins(&aube_dir, "host@1.0.0", "host", &[("x", "own.js")]);
        materialize_pkg_with_bins(&aube_dir, "childx@1.0.0", "childx", &[("x", "child.js")]);
        link_child_sibling(&aube_dir, &host_dir, "host", "childx@1.0.0", "childx");

        // (c) No dependencies at all — the pass used to skip these outright.
        let solo_dir =
            materialize_pkg_with_bins(&aube_dir, "solo@1.0.0", "solo", &[("solo", "s.js")]);

        let mut tool = locked("@scope/tool", "1.0.0", BTreeMap::new());
        tool.dep_path = "@scope/tool@1.0.0".to_string();
        tool.dependencies
            .insert("helper".to_string(), "1.0.0".to_string());
        let mut host = locked("host", "1.0.0", BTreeMap::new());
        host.dependencies
            .insert("childx".to_string(), "1.0.0".to_string());

        let mut packages = BTreeMap::new();
        packages.insert("@scope/tool@1.0.0".to_string(), tool);
        packages.insert(
            "helper@1.0.0".to_string(),
            locked("helper", "1.0.0", BTreeMap::new()),
        );
        packages.insert("host@1.0.0".to_string(), host);
        packages.insert(
            "childx@1.0.0".to_string(),
            locked("childx", "1.0.0", BTreeMap::new()),
        );
        packages.insert(
            "solo@1.0.0".to_string(),
            locked("solo", "1.0.0", BTreeMap::new()),
        );
        let graph = LockfileGraph {
            packages,
            ..Default::default()
        };

        // The isolated linker's real options: a shell/cmd wrapper, not a bare
        // symlink, so the emitted relative target is readable as text on every
        // platform (Windows writes the extensionless sh wrapper beside the
        // `.cmd` / `.ps1` pair).
        let shim_opts = aube_linker::BinShimOptions {
            prefer_symlinked_executables: Some(false),
            ..Default::default()
        };
        link_dep_bins(
            &aube_dir,
            &graph,
            120,
            None,
            shim_opts,
            &mut PkgJsonCache::new(),
        )
        .unwrap();

        let bin_of =
            |pkg_dir: &std::path::Path, name: &str| dep_modules_dir_for(pkg_dir, name).join(".bin");

        let tool_bin = bin_of(&tool_dir, "@scope/tool");
        assert!(
            tool_bin.join("helper").exists(),
            "control: the child's bin must still be linked into the package's own \
             `.bin`; without it the self-bin asserts below prove nothing"
        );
        assert!(
            tool_bin.join("tool-cli").exists(),
            "a scoped package's own bin must appear in the `.bin` beside it, so a \
             postinstall deriving that path from __dirname finds its own entry \
             (expected {})",
            tool_bin.join("tool-cli").display()
        );

        let host_bin = bin_of(&host_dir, "host");
        let x = std::fs::read_to_string(host_bin.join("x")).unwrap();
        assert!(
            x.contains("child.js") && !x.contains("own.js"),
            "a name already claimed by a child must NOT be displaced by the \
             self-bin pass — `x` should still resolve to the child's target; got:\n{x}"
        );

        let solo_bin = bin_of(&solo_dir, "solo");
        assert!(
            solo_bin.join("solo").exists(),
            "a package with NO dependencies must still get its own bin linked \
             (expected {})",
            solo_bin.join("solo").display()
        );

        // (d) THE RELINK MUST REFRESH ITS OWN EARLIER WRITE. `link_all_bins`
        //     runs again after dep lifecycle scripts precisely so a bin the
        //     build replaced gets re-classified (#394). Stand in for that build
        //     by turning the target into a native executable, then re-run the
        //     pass exactly as `finalize.rs` does.
        let tool_target = tool_dir.join("bin/tool");
        std::fs::write(&tool_target, b"\x7FELF\x02\x01\x01\x00 native now").unwrap();
        link_dep_bins(
            &aube_dir,
            &graph,
            120,
            None,
            shim_opts,
            &mut PkgJsonCache::new(),
        )
        .unwrap();
        let refreshed = std::fs::read_to_string(tool_bin.join("tool-cli")).unwrap();
        assert!(
            !refreshed.contains("node"),
            "the post-build pass must re-classify a self-bin whose target turned \
             native and emit a DIRECT exec; a shim still handing the binary to \
             `node` dies `SyntaxError: Invalid or unexpected token`. Got:\n{refreshed}"
        );
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
        link_hoisted_placement_bins(&graph, &placements, shim_opts, &mut cache).unwrap();
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
