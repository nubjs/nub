use super::{AddArgs, run};
use miette::{Context, IntoDiagnostic};
use std::collections::BTreeMap;

/// `aube add -g <pkg>...` — install into an isolated global install dir
/// and symlink the resulting binaries into the global bin dir.
///
/// The project-local `run` path assumes a `package.json` in the cwd. The
/// global path deliberately does *not* — it creates a fresh install dir
/// under `<pkg_dir>/<pid>-<ts>`, writes a minimal `package.json` so the
/// normal install pipeline has something to resolve against, chdirs into
/// it, and then re-enters `run` with the local flow. After the install
/// lands we scan the install dir's `node_modules/.bin/` and symlink each
/// bin into `<bin_dir>`.
///
/// The freshly-created install dir is cleaned up if *any* step after
/// creation fails — inner install, manifest re-read, hash pointer, or
/// bin linking. Without this guard every failed `add -g` would leak a
/// subdir that `scan_packages` ignores (no hash symlink) but disk space
/// keeps.
pub(super) async fn run_global(
    packages: &[String],
    allow_build: Vec<String>,
    deny_build: Vec<String>,
    allow_low_downloads: bool,
    lockfile: crate::cli_args::LockfileArgs,
    network: crate::cli_args::NetworkArgs,
    virtual_store: crate::cli_args::VirtualStoreArgs,
) -> miette::Result<()> {
    use crate::commands::global;

    let mut layout = global::GlobalLayout::resolve()?;
    let install_dir_raw = global::create_install_dir(&layout.pkg_dir)?;

    // Canonicalize both the install dir and the layout's pkg dir so the
    // comparisons downstream (`find_package`, `remove_package`) see the
    // same form regardless of filesystem-level symlinks. On macOS the
    // default temp dir `/var/folders/...` is itself a symlink to
    // `/private/var/folders/...`, and `scan_packages` always canonicalizes
    // the hash-symlink targets — so without normalizing our side the
    // `!=` / `starts_with` checks all come out wrong and we either leak
    // orphan install dirs or leave duplicate hash pointers behind.
    // Use `dirs::canonicalize` (not `std::fs::canonicalize`) so the
    // result on Windows is a plain drive path, not the `\\?\C:\…`
    // verbatim form. `link_bin_entries` later concatenates this dir
    // into the relative bin-shim target via `%~dp0\{rel}`; a verbatim
    // prefix in `{rel}` produces a path neither `cmd.exe` nor Node can
    // resolve and surfaces as `Cannot find module '<bin>\?\<target>'`.
    let install_dir = crate::dirs::canonicalize(&install_dir_raw)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to canonicalize install dir {}",
                install_dir_raw.display()
            )
        })?;
    if let Ok(canon) = crate::dirs::canonicalize(&layout.pkg_dir) {
        layout.pkg_dir = canon;
    }

    // Everything from here until the final `Ok(())` must run under a
    // cleanup guard so a mid-flight failure doesn't leave an orphan dir
    // or a dangling hash pointer under the global pkg dir. We snapshot
    // the pkg dir's existing hash pointers before running, then on
    // error remove any new pointers that appeared plus the install dir.
    let before: std::collections::HashSet<std::path::PathBuf> = std::fs::read_dir(&layout.pkg_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    let result = run_global_inner(
        packages,
        allow_build,
        deny_build,
        allow_low_downloads,
        &layout,
        &install_dir,
        lockfile,
        network,
        virtual_store,
    )
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&install_dir);
        if let Ok(entries) = std::fs::read_dir(&layout.pkg_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if before.contains(&path) {
                    continue;
                }
                // Only unlink pointers that resolved to our install dir —
                // don't touch pointers for other live global installs.
                // Use `dirs::canonicalize` so the equality check against
                // `install_dir` (also stripped of any Windows `\\?\`
                // verbatim prefix) actually matches.
                if let Ok(target) = crate::dirs::canonicalize(&path)
                    && target == install_dir
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_global_inner(
    packages: &[String],
    allow_build: Vec<String>,
    deny_build: Vec<String>,
    allow_low_downloads: bool,
    layout: &crate::commands::global::GlobalLayout,
    install_dir: &std::path::Path,
    lockfile: crate::cli_args::LockfileArgs,
    network: crate::cli_args::NetworkArgs,
    virtual_store: crate::cli_args::VirtualStoreArgs,
) -> miette::Result<()> {
    use crate::commands::global;

    // Seed a minimal package.json so the resolver has a project to work
    // against. We never persist metadata beyond this; the install dir is
    // throwaway and lives only to host `node_modules/`.
    let seed = serde_json::json!({
        "name": "aube-global",
        "version": "0.0.0",
        "private": true,
    });
    let seed_str = serde_json::to_string_pretty(&seed)
        .into_diagnostic()
        .wrap_err("failed to serialize seed package.json")?;
    aube_util::fs_atomic::atomic_write(
        &install_dir.join("package.json"),
        format!("{seed_str}\n").as_bytes(),
    )
    .into_diagnostic()
    .wrap_err("failed to write seed package.json")?;

    // chdir into the install dir before anything reads `dirs::cwd()` so
    // the whole install pipeline targets the fresh directory. See the
    // invariant note on `run_global` above — this works only because
    // nothing upstream has called `dirs::cwd()` yet.
    std::env::set_current_dir(install_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chdir into {}", install_dir.display()))?;
    crate::dirs::set_cwd(install_dir)?;

    // Build registry map before the inner `run` takes its own view of
    // the config — we need it for the cache key hash.
    let npm_config = aube_registry::config::NpmConfig::load(install_dir);
    let mut registries: BTreeMap<String, String> = BTreeMap::new();
    registries.insert("default".to_string(), npm_config.registry.clone());
    for (scope, url) in &npm_config.scoped_registries {
        registries.insert(scope.clone(), url.clone());
    }

    // Re-enter the local add path inside the throwaway project. Global
    // installs pin the exact resolved version — matches pnpm's
    // `pnpm add -g` behavior (no `^` in the synthetic manifest) and
    // keeps the cache key stable across re-adds.
    let inner = AddArgs {
        packages: packages.to_vec(),
        save_dev: false,
        save_exact: true,
        global: false,
        save_optional: false,
        ignore_scripts: false,
        no_save: false,
        save_peer: false,
        save_workspace_protocol: false,
        no_save_workspace_protocol: false,
        // The throwaway install dir is never a workspace root, but
        // `run_global_inner` is the one place in aube that chdirs
        // after startup — if a future refactor reads `dirs::cwd()`
        // before command dispatch the synthetic `AddArgs` could end
        // up being evaluated against the *caller's* cwd. Opting out
        // of the check here keeps `aube add -g` robust against that
        // regression without relying on the chdir-ordering invariant.
        ignore_workspace_root_check: true,
        workspace: false,
        save_catalog: false,
        save_catalog_name: None,
        // Forward `--allow-build=<pkg>` flags from the outer invocation:
        // the inner `run()` writes them to the throwaway install dir's
        // `package.json#aube.allowBuilds` (no workspace yaml exists
        // there) before lifecycle scripts run, matching pnpm's
        // `pnpm add -g --allow-build=<pkg>` behavior. Without this the
        // outer flag is silently dropped — under `strictDepBuilds=true`
        // the install then errors with "must be reviewed before
        // install" even when the user explicitly approved the dep
        // (see Discussion #617).
        allow_build,
        // Same contract as `allow_build`, but force-denies matching
        // packages so global installs can satisfy `strictDepBuilds=true`
        // while keeping selected lifecycle scripts skipped.
        deny_build,
        // The synthetic inner `run()` is the one that actually fires
        // the supply-chain gate (`add` only checks once, in the main
        // path). Forward the outer caller's `--allow-low-downloads`
        // through so `aube add -g --allow-low-downloads <pkg>` skips
        // the prompt as expected.
        allow_low_downloads,
        // Propagate the outer caller's flag groups through so the inner
        // run()'s `install_overrides()` calls reset the global slots to the
        // same values rather than wiping them — `set_registry_override`
        // backs an RwLock that always overwrites, unlike the OnceLock
        // siblings, so a `Default` here would silently drop `--registry`.
        lockfile,
        network,
        virtual_store,
    };
    Box::pin(run(
        inner,
        aube_workspace::selector::EffectiveFilter::default(),
    ))
    .await?;

    // Re-read the install dir's package.json to get the resolved alias
    // list. Anything in `dependencies` at this point was added by the
    // inner run; we stamp a hash pointer on that set.
    let manifest_raw = std::fs::read_to_string(install_dir.join("package.json"))
        .into_diagnostic()
        .wrap_err("failed to re-read install dir package.json")?;
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_raw)
        .into_diagnostic()
        .wrap_err("failed to parse install dir package.json")?;
    let aliases: Vec<String> = manifest_json
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    // Commit the new install *before* tearing down any prior ones. If
    // the hash pointer or bin-link step fails, the outer cleanup guard
    // still wipes the new install dir, but the user's previous global
    // install is untouched — they're never left without a working copy.
    // Capture every prior install whose pointer (or aliases) overlaps
    // the new one *before* we touch the filesystem. We can't scan for
    // priors after the new pointer lands, because the overwrite loses
    // the previous target — `find_package` would return our fresh
    // install instead. Two kinds of prior matter:
    //
    // 1. The install a same-hash pointer used to point at (the caller
    //    re-ran `add -g` with the exact same alias set).
    // 2. Installs that own one of the new aliases under a *different*
    //    hash (alias set grew/shrank).
    let hash = global::cache_key(&aliases, &registries);
    let hash_ptr = global::hash_link(&layout.pkg_dir, &hash);
    let mut priors: Vec<global::GlobalPackageInfo> = Vec::new();
    // `dirs::canonicalize` for the same Windows-prefix reason as above —
    // we compare against `install_dir`, which is itself stripped.
    if let Ok(existing_target) = crate::dirs::canonicalize(&hash_ptr)
        && existing_target != install_dir
    {
        priors.extend(
            global::scan_packages(&layout.pkg_dir)
                .into_iter()
                .filter(|p| p.install_dir == existing_target),
        );
    }
    for alias in &aliases {
        if let Some(existing) = global::find_package(&layout.pkg_dir, alias)
            && existing.install_dir != install_dir
            && existing.hash != hash
            && !priors.iter().any(|p| p.hash == existing.hash)
        {
            priors.push(existing);
        }
    }

    // Commit the new install *before* tearing down the priors. If the
    // hash pointer or bin-link step fails, the outer cleanup guard
    // wipes the new install dir but the priors survive — users never
    // end up with no working copy.
    global::symlink_force(install_dir, &hash_ptr)?;
    // Honor extendNodePath / preferSymlinkedExecutables for global bins too —
    // settings resolved from the user's `.npmrc` via the normal cwd-walking
    // chain starting at the throwaway install dir, which lives under
    // `~/.aube/global/` and will still pick up the user-level `.npmrc`.
    // `hidden_modules_dir = None`: the global install lays packages out
    // at `<install>/node_modules/<name>` (hoisted shape, no `.aube/`
    // store), so the bin's `$basedir/..` already lands the resolver on
    // every transitive — no second NODE_PATH entry needed.
    let shim_opts =
        crate::commands::with_settings_ctx(install_dir, |ctx| aube_linker::BinShimOptions {
            extend_node_path: aube_settings::resolved::extend_node_path(ctx),
            prefer_symlinked_executables: aube_settings::resolved::prefer_symlinked_executables(
                ctx,
            ),
            hidden_modules_dir: None,
        });
    let linked = global::link_bins(install_dir, &layout.bin_dir, &aliases, shim_opts)?;

    // Now safe to drop priors. Errors here are non-fatal — the new
    // install is already live — but we still surface them so the user
    // knows they have leftover state.
    //
    // If a prior shares the new hash, its pointer is already pointing
    // at the *new* install dir (we overwrote it a few lines up). Deleting
    // the pointer in that case would break the live install, so we only
    // wipe the prior's physical dir + bins.
    for prior in &priors {
        let res = if prior.hash == hash {
            let bins = global::bin_names_for(&prior.install_dir, &prior.aliases);
            global::unlink_bins(&prior.install_dir, &layout.bin_dir, &bins);
            std::fs::remove_dir_all(&prior.install_dir)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .map_err(|e| {
                    miette::miette!(
                        code = aube_codes::errors::ERR_AUBE_REMOVE_PRIOR_INSTALL_DIR,
                        "failed to remove prior install dir: {e}"
                    )
                })
        } else {
            global::remove_package(prior, layout)
        };
        if let Err(e) = res {
            eprintln!("warning: failed to remove prior global install: {e}");
        }
    }

    if !linked.is_empty() {
        eprintln!(
            "Linked {} into {}",
            pluralizer::pluralize("bin", linked.len() as isize, true),
            layout.bin_dir.display()
        );
    }

    Ok(())
}
