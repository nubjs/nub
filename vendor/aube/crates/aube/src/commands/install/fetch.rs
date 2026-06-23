use super::critical_path::is_likely_native_build;
use super::git_prepare::{prepare_scratch_copy, run_git_dep_prepare};
use super::lifecycle::run_import_on_blocking;
use super::settings::{
    default_lockfile_network_concurrency, resolve_network_concurrency,
    resolve_strict_store_integrity, resolve_strict_store_pkg_content_check,
    resolve_verify_store_integrity,
};
use crate::commands::{
    packument_cache_dir, resolve_virtual_store_dir, resolve_virtual_store_dir_max_length,
};
use crate::progress::InstallProgress;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use miette::{Context, IntoDiagnostic, miette};
use rayon::prelude::*;
use std::collections::BTreeMap;

/// Materialize a local-source package into the store.
///
/// `Directory` walks the target and hash-imports every file; `Tarball`
/// opens the `.tgz` and reuses the normal tarball importer. `Link`
/// returns `None` because link deps never have a store-backed index —
/// the linker symlinks directly to the target in step 2. `Portal`
/// imports the target package like a directory so its dependency graph
/// is linked, and `Exec` runs the generator into a temp build dir.
#[allow(clippy::too_many_arguments)]
pub(super) async fn import_local_source(
    store: &std::sync::Arc<aube_store::Store>,
    project_root: &std::path::Path,
    local: &aube_lockfile::LocalSource,
    client: Option<&std::sync::Arc<aube_registry::client::RegistryClient>>,
    ignore_scripts: bool,
    git_prepare_depth: u32,
    inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    git_shallow_hosts: &[String],
    pkg_name: &str,
    pkg_version: &str,
) -> miette::Result<Option<aube_store::PackageIndex>> {
    // `chain` is appended to per-error messages below so users see
    // *why* a `file:` / `link:` / git / remote-tarball dep was pulled
    // in. Empty when the package isn't in the resolved chain index
    // (e.g. when the install pipeline hasn't seeded one yet for an
    // out-of-band caller).
    let chain = crate::dep_chain::format_chain_for(pkg_name, pkg_version);
    use aube_lockfile::LocalSource;
    match local {
        LocalSource::Link(_) => Ok(None),
        LocalSource::Directory(rel) | LocalSource::Portal(rel) => {
            let abs = project_root.join(rel);
            if !abs.is_dir() {
                return Err(miette!(
                    "local dependency {}: {} is not a directory{chain}",
                    local.specifier(),
                    abs.display()
                ));
            }
            let index = store
                .import_directory(&abs)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
        LocalSource::Exec(_) => {
            if ignore_scripts {
                return Err(miette!(
                    "{} requires executing its generator, but scripts are disabled{chain}",
                    local.specifier()
                ));
            }
            let script = aube_resolver::resolve_exec_script_path(local, project_root)
                .map_err(|e| miette!("exec dependency {}: {e}{chain}", local.specifier()))?;
            let temp = tempfile::Builder::new()
                .prefix("aube-exec-")
                .tempdir()
                .into_diagnostic()
                .wrap_err_with(|| format!("create temp dir for {}{chain}", local.specifier()))?;
            let build_dir = temp.path().join("build");
            let temp_dir = temp.path().join("temp");
            std::fs::create_dir_all(&build_dir)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!("create exec build dir for {}{chain}", local.specifier())
                })?;
            std::fs::create_dir_all(&temp_dir)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!("create exec temp dir for {}{chain}", local.specifier())
                })?;
            let env = serde_json::json!({
                "tempDir": temp_dir,
                "buildDir": build_dir,
                "locator": format!("{pkg_name}@{}", local.specifier()),
            });
            let status = tokio::process::Command::new(crate::runtime::node_program())
                .arg("-e")
                .arg(aube_resolver::YARN_EXEC_WRAPPER)
                .arg(&script)
                .env("AUBE_YARN_EXEC_ENV", env.to_string())
                .current_dir(project_root)
                .status()
                .await
                .map_err(|e| {
                    miette!(
                        "execute {} with Node.js from PATH: {e}{chain}",
                        local.specifier()
                    )
                })?;
            if !status.success() {
                return Err(miette!(
                    "exec dependency {} failed with status {status}{chain}",
                    local.specifier()
                ));
            }
            let index = store.import_directory(&build_dir).map_err(|e| {
                miette!(
                    "failed to import generated {}: {e}{chain}",
                    local.specifier()
                )
            })?;
            Ok(Some(index))
        }
        LocalSource::Tarball(rel) => {
            let abs = project_root.join(rel);
            let bytes = std::fs::read(&abs)
                .into_diagnostic()
                .wrap_err_with(|| format!("read {}{chain}", abs.display()))?;
            let index = store
                .import_tarball(&bytes)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
        LocalSource::Git(g) => {
            // Materialize the git dep into a commit-keyed cache
            // directory and hardlink-import into the store exactly
            // like a `file:` directory. The resolver already pinned
            // `g.resolved` to a full commit SHA, so we route through
            // the same hosted-tarball-then-clone fallback npm and
            // pnpm use:
            //
            //   1. github / gitlab / bitbucket public reads → a flat
            //      HTTPS tarball over codeload (no `git` binary, no
            //      SSH key required).
            //   2. Anything else, plus codeload errors → shallow
            //      `git clone` over HTTPS (rewritten from the stored
            //      lockfile URL when the host is hosted, so an
            //      `git+ssh://git@github.com/…` lockfile still works
            //      on a host with no SSH key).
            //   3. Non-hosted hosts → unchanged: clone whatever URL
            //      the lockfile recorded, preserving SSH-only setups.
            //
            // Both the codeload extract and the clone share the
            // `(url, commit)` cache so the resolver's earlier call
            // for the same dep doesn't pay the network round-trip
            // twice.
            let url = g.url.clone();
            let resolved = g.resolved.clone();
            let spec = local.specifier();
            let hosted = aube_lockfile::parse_hosted_git(&url);
            let runtime_url = hosted
                .as_ref()
                .map(|h| h.https_url())
                .unwrap_or_else(|| url.clone());
            let codeload_url = hosted.as_ref().and_then(|h| h.tarball_url(&resolved));

            // Cache hit fast path: skip the HTTPS round-trip when the
            // resolver already populated the codeload cache for this
            // (url, commit) pair earlier in the install. Mirrors
            // `git_shallow_clone`'s top-of-function reuse check.
            let mut clone_dir: Option<std::path::PathBuf> =
                if codeload_url.is_some() && g.integrity.is_some() {
                    aube_store::codeload_cache_lookup(&url, &resolved, g.integrity.as_deref())
                        .map(|(dir, _)| dir)
                } else {
                    None
                };
            if clone_dir.is_none()
                && let (Some(c), Some(url_to_fetch)) = (client, codeload_url.as_deref())
            {
                match c.fetch_tarball_bytes(url_to_fetch).await {
                    Ok(bytes) => {
                        let bytes_vec = bytes.to_vec();
                        if let Some(pinned) = &g.integrity {
                            aube_store::verify_integrity(&bytes_vec, pinned)
                                .map_err(|e| miette!("{spec}: {e}{chain}"))?;
                        }
                        let integrity = g
                            .integrity
                            .clone()
                            .unwrap_or_else(|| aube_store::sha512_integrity(&bytes_vec));
                        let url_for_extract = url.clone();
                        let resolved_for_extract = resolved.clone();
                        let integrity_for_extract = integrity.clone();
                        match tokio::task::spawn_blocking(move || {
                            aube_store::extract_codeload_tarball(
                                &bytes_vec,
                                &url_for_extract,
                                &resolved_for_extract,
                                Some(&integrity_for_extract),
                            )
                        })
                        .await
                        .map_err(|e| miette!("codeload extract task panicked: {e}"))?
                        {
                            Ok((dir, _sha)) => clone_dir = Some(dir),
                            Err(e) => {
                                tracing::debug!(
                                    %spec,
                                    "codeload extract failed, falling back to git clone: {e}",
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            %spec,
                            url = %aube_util::url::redact_url(url_to_fetch),
                            "codeload fetch failed, falling back to git clone: {e}",
                        );
                    }
                }
            }

            let clone_dir = if let Some(dir) = clone_dir {
                dir
            } else {
                let shallow = aube_store::git_host_in_list(&runtime_url, git_shallow_hosts);
                let url_for_clone = runtime_url.clone();
                let resolved_for_clone = resolved.clone();
                let (dir, _head_sha) = tokio::task::spawn_blocking(move || {
                    aube_store::git_shallow_clone(&url_for_clone, &resolved_for_clone, shallow)
                })
                .await
                .map_err(|e| miette!("git clone task panicked: {e}{chain}"))?
                .map_err(|e| miette!("failed to clone {spec}: {e}{chain}"))?;
                dir
            };

            // `&path:/<sub>` narrows the package root to a
            // subdirectory of the cloned repo (pnpm-compatible).
            // Everything below this line — manifest read, prepare
            // scratch copy, archive build, plain directory import —
            // operates on the subdir rather than the whole clone.
            //
            // Defense in depth against a `..`-laden subpath: the
            // parser already rejects them, but we also canonicalize
            // and assert the result stays under `clone_dir` so a
            // future code path that fills `subpath` from a different
            // source can't bypass the check.
            let pkg_root = match &g.subpath {
                Some(sub) => clone_dir.join(sub),
                None => clone_dir.clone(),
            };
            if !pkg_root.is_dir() {
                return Err(miette!(
                    "git dep {spec}: subpath {} not found in clone{chain}",
                    pkg_root.display()
                ));
            }
            if g.subpath.is_some() {
                let canonical_clone = clone_dir
                    .canonicalize()
                    .into_diagnostic()
                    .wrap_err_with(|| format!("canonicalize clone dir for {spec}{chain}"))?;
                let canonical_pkg = pkg_root
                    .canonicalize()
                    .into_diagnostic()
                    .wrap_err_with(|| format!("canonicalize subpath for {spec}{chain}"))?;
                if !canonical_pkg.starts_with(&canonical_clone) {
                    return Err(miette!(
                        "git dep {spec}: subpath {} escapes clone root {}{chain}",
                        canonical_pkg.display(),
                        canonical_clone.display()
                    ));
                }
            }

            // If the cloned repo defines a `prepare` script, treat
            // it as a source checkout that needs to be built before
            // we snapshot it. Matches npm/pnpm: a TypeScript repo
            // installed from git has devDependencies + a `prepare`
            // that compiles `src/` into `dist/`, and consumers
            // expect the built output. We run a nested `aube
            // install` inside the clone, which installs its deps
            // and runs its own root lifecycle hooks (including
            // `prepare`), then `aube pack`'s file-selection logic
            // snapshots exactly what would be published (honors
            // `files`, `.npmignore`, and skips `node_modules`).
            //
            // `--ignore-scripts` short-circuits the whole branch:
            // the only reason we'd pay the cost of a nested install
            // is to run `prepare`, so with scripts disabled we fall
            // through to the plain directory import. Matches pnpm,
            // which skips `prepare` for git deps under
            // `--ignore-scripts` as well.
            let manifest_path = pkg_root.join("package.json");
            let needs_prepare = !ignore_scripts
                && aube_manifest::PackageJson::from_path(&manifest_path)
                    .ok()
                    .is_some_and(|pj| pj.scripts.contains_key("prepare"));

            if needs_prepare {
                // Run `prepare` on a private copy of the checkout,
                // not on the shared `git_shallow_clone` cache
                // directory. The cache is keyed by (url, commit)
                // and reused across installs; mutating it in place
                // would leave `node_modules/`, `aube-lock.yaml`,
                // and any generated `dist/` behind, so a later
                // `aube install --ignore-scripts` — which falls
                // through to the plain directory-import path —
                // would silently pull those build artifacts into
                // the store even though the user asked for a
                // scripts-free install. Copying also isolates
                // concurrent installs of the same git dep from
                // clobbering each other's in-progress prepare.
                //
                // `ScratchDir` removes the copy on drop, including
                // on the error path.
                let scratch = prepare_scratch_copy(&pkg_root, &spec)?;
                run_git_dep_prepare(
                    scratch.path(),
                    &spec,
                    ignore_scripts,
                    git_prepare_depth,
                    inherited_build_policy,
                )
                .await?;
                let archive = crate::commands::pack::build_archive(scratch.path())
                    .wrap_err_with(|| format!("failed to pack prepared git dep {spec}{chain}"))?;
                let index = store
                    .import_tarball(&archive.tarball)
                    .map_err(|e| miette!("failed to import prepared {spec}: {e}{chain}"))?;
                return Ok(Some(index));
            }

            let index = store
                .import_directory(&pkg_root)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
        LocalSource::RemoteTarball(t) => {
            // Remote tarball URL: download once, verify the
            // resolver-pinned integrity, and import like any other
            // .tgz. Reuses the normal tarball importer so the
            // linker sees a plain PackageIndex. No store-level
            // index cache lookup — the canonical key would need to
            // be `(url, integrity)` rather than `(name, version)`
            // and remote tarball deps are rare enough that the
            // redundant walk isn't worth a new cache namespace.
            let client = client.ok_or_else(|| {
                miette!(
                    "internal: import_local_source called without a registry client for {}{chain}",
                    local.specifier()
                )
            })?;
            let bytes = client
                .fetch_tarball_bytes(&t.url)
                .await
                .map_err(|e| miette!("failed to fetch {}: {e}{chain}", t.url))?;
            if t.integrity.is_empty() {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_MISSING_INTEGRITY,
                    url = %aube_util::url::redact_url(&t.url),
                    "remote tarball lockfile entry has no integrity field; importing fetched bytes without verification (run `{} --no-frozen-lockfile` to refresh the lockfile)",
                    aube_util::cmd("install"),
                );
            } else {
                aube_store::verify_integrity(&bytes, &t.integrity)
                    .map_err(|e| miette!("{}: {e}{chain}", aube_util::url::redact_url(&t.url)))?;
            }
            let index = store
                .import_tarball(&bytes)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
    }
}

/// Fetch tarballs for resolved packages, checking the index cache first.
/// Used by the lockfile path where all packages are known upfront.
/// Exposed to sibling commands so `aube fetch` can reuse the same
/// parallel-download + integrity-check + index-cache pipeline.
pub(in crate::commands) async fn fetch_packages(
    packages: &BTreeMap<String, aube_lockfile::LockedPackage>,
    store: &std::sync::Arc<aube_store::Store>,
    client: std::sync::Arc<aube_registry::client::RegistryClient>,
    progress: Option<&InstallProgress>,
    ignore_scripts: bool,
    git_prepare_depth: u32,
    git_shallow_hosts: Vec<String>,
) -> miette::Result<(BTreeMap<String, aube_store::PackageIndex>, usize, usize)> {
    // Eager-client caller (`aube fetch`): the command only exists to
    // download tarballs, so there's no point deferring construction.
    // `skip_already_linked_shortcut=true` because `aube fetch`'s entire
    // job is to verify/populate the global store — it must not be
    // short-circuited by a stale `node_modules/.aube/<dep>` from a
    // prior install, which could leave the store empty on a setup
    // that wipes the global aube store but not `node_modules/` (e.g.
    // Docker layer caching, where the store lives in one cached
    // layer and `node_modules` in another).
    let cwd = crate::dirs::project_root_or_cwd()?;
    // `aube fetch` is a thin wrapper whose only job is populating
    // the store, so resolve `networkConcurrency` and
    // `verifyStoreIntegrity` from the project context here and hand
    // them down. Doing the resolve in the wrapper (instead of in
    // `aube fetch`'s own entry point) keeps the two call paths
    // honest: the lockfile install path and the standalone fetch
    // path share the same hardcoded fallback behavior when no
    // setting is configured.
    let files = crate::commands::FileSources::load(&cwd);
    let raw_workspace = aube_manifest::workspace::load_both(&cwd)
        .map(|(_, raw)| raw)
        .unwrap_or_default();
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    let network_concurrency = resolve_network_concurrency(&ctx);
    let verify_integrity = resolve_verify_store_integrity(&ctx);
    let strict_integrity = resolve_strict_store_integrity(&ctx);
    let strict_pkg_content_check = resolve_strict_store_pkg_content_check(&ctx);
    let virtual_store_dir_max_length = resolve_virtual_store_dir_max_length(&ctx);
    let aube_dir = resolve_virtual_store_dir(&ctx, &cwd);
    fetch_packages_with_root(
        packages,
        store,
        || client,
        progress,
        &cwd,
        &aube_dir,
        /*materialize_tx=*/ None,
        /*skip_already_linked_shortcut=*/ true,
        virtual_store_dir_max_length,
        ignore_scripts,
        network_concurrency,
        verify_integrity,
        strict_integrity,
        strict_pkg_content_check,
        git_prepare_depth,
        None,
        git_shallow_hosts,
    )
    .await
}

// `network_concurrency`: override for the tarball-fetch semaphore.
//   `None` uses the built-in default (128). Surfaced so the
//   `networkConcurrency` setting, resolved once at the install-run
//   entry point, can cap parallel downloads.
// `verify_integrity`: whether to verify each tarball's SHA-512 against
//   its lockfile integrity before importing into the store. `false`
//   skips the check entirely; corresponds to `verifyStoreIntegrity=false`.
// `strict_pkg_content_check`: whether to validate that the imported
//   tarball's `package.json` advertises the same (name, version) the
//   resolver requested. `true` (pnpm default) rejects mismatches before
//   linking; corresponds to `strictStorePkgContentCheck=true`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn fetch_packages_with_root<F>(
    packages: &BTreeMap<String, aube_lockfile::LockedPackage>,
    store: &std::sync::Arc<aube_store::Store>,
    client: F,
    progress: Option<&InstallProgress>,
    project_root: &std::path::Path,
    aube_dir: &std::path::Path,
    // Some streams every successful (dep_path, index) so a concurrent
    // GVS-prewarm materializer can start reflinks before the full
    // batch finishes. None keeps batch-then-return for `aube fetch`.
    // Sender drops on function exit so consumer sees channel close.
    materialize_tx: Option<tokio::sync::mpsc::Sender<(String, aube_store::PackageIndex)>>,
    // When true, every package classifies as `Cached` or `NeedsFetch`
    // based on `store.load_index`, regardless of whether
    // `.aube/<dep>` already exists on disk. Callers pass true when
    // either:
    //
    //   - the linker will wipe `node_modules/` before running
    //     (`link_workspace`), so the `AlreadyLinked` classification
    //     would be immediately invalidated; or
    //   - the caller needs `load_index` to actually run as its store
    //     verification step (`aube fetch`, which treats the act of
    //     walking the store-file existence check as the operation's
    //     primary side effect).
    //
    // Both cases share the same implementation: skip the `.aube/`
    // existence check entirely so every package goes through
    // `store.load_index` → either `Cached` (store has it) or
    // `NeedsFetch` (store is missing the file, download fresh).
    skip_already_linked_shortcut: bool,
    virtual_store_dir_max_length: usize,
    ignore_scripts: bool,
    network_concurrency: Option<usize>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
    git_prepare_depth: u32,
    inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    git_shallow_hosts: Vec<String>,
) -> miette::Result<(BTreeMap<String, aube_store::PackageIndex>, usize, usize)>
where
    F: FnOnce() -> std::sync::Arc<aube_registry::client::RegistryClient>,
{
    // No-op fast path: for every package whose per-project
    // `node_modules/.aube/<dep_path>` entry already resolves to an
    // existing target, skip the package-index load entirely. The
    // linker's only consumer of a `PackageIndex` is
    // `materialize_into` — if the package is already materialized
    // (either as a real directory here in per-project mode, or as a
    // symlink into the global virtual store that itself exists),
    // there's nothing to materialize and the 13–15 KB JSON on disk at
    // `<store>/v1/index/<name>@<ver>.json` would be read for
    // nothing. A fresh no-op install against the 1.4k-package medium
    // fixture drops from ~38 ms of parallel index reads to a handful
    // of `stat(2)`s.
    //
    // Two call sites disable the fast path entirely via
    // `skip_already_linked_shortcut=true`:
    //
    //   - **Workspace installs.** `link_workspace` unconditionally
    //     wipes `node_modules/` (including `.aube/`) before
    //     rebuilding, so every `AlreadyLinked` classification would
    //     be invalidated by the time the linker runs. With the fast
    //     path enabled, the linker would then fall back to
    //     `self.store.load_index` *serially* inside `link_workspace`'s
    //     for-loop, which is strictly slower than loading them here
    //     in parallel via rayon.
    //
    //   - **`aube fetch`.** The command exists to populate the
    //     global store (typical use: Docker layer caching, warming
    //     a CI mirror, or recovering from a wiped aube store).
    //     If `node_modules/.aube/<dep>` happens to exist from a
    //     previous install, the `AlreadyLinked` shortcut would skip
    //     both `load_index` and the tarball fetch — which silently
    //     leaves the store empty even though the user explicitly
    //     asked for it to be repopulated. Disabling the shortcut
    //     makes every package flow through `store.load_index`,
    //     which does a first-file existence check on the CAS and
    //     correctly downgrades to `NeedsFetch` when the store entry
    //     has been wiped.
    //
    // `Path::exists` follows symlinks, so a per-project entry pointing
    // at a global virtual-store target that no longer exists correctly
    // falls through to the slow path. The linker re-derives the entry
    // name through `aube_dir_entry_name(dep_path)`, which is just
    // `dep_path_to_filename(dep_path, max_length)` — we take the max
    // length as a parameter (instead of reaching for
    // `DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH`) so the fast path checks
    // the exact same filename the linker will write. The install
    // driver (and the `aube fetch` wrapper) both resolve this through
    // `super::resolve_virtual_store_dir_max_length(&ctx)` so user
    // overrides of `virtualStoreDirMaxLength` flow to both sites and
    // we can't produce "the fast path saw `.aube/<X>` but the linker
    // expected `.aube/<Y>`" bugs on dep_paths long enough to trigger
    // the truncate-and-hash fallback inside `dep_path_to_filename`.
    // `aube_dir` is threaded in from
    // `commands::resolve_virtual_store_dir` so custom `virtualStoreDir`
    // values land on the same path the linker will write to.

    enum CheckResult {
        /// Already linked into `node_modules/.aube/<dep_path>`. The
        /// linker won't need the package index for this dep at all.
        AlreadyLinked,
        /// Store has the index; linker will reuse it to (re)create any
        /// missing symlinks.
        Cached(aube_store::PackageIndex),
        /// Missing from the store — falls through to the tarball fetch.
        NeedsFetch,
    }

    // Parallel index check (rayon)
    let check_results: Vec<_> = packages
        .par_iter()
        .filter(|(_, pkg)| pkg.local_source.is_none())
        .map(|(dep_path, pkg)| {
            if !skip_already_linked_shortcut {
                let entry_name = dep_path_to_filename(dep_path, virtual_store_dir_max_length);
                if aube_dir.join(&entry_name).exists() {
                    return (dep_path.clone(), pkg, CheckResult::AlreadyLinked);
                }
            }
            // Keyed by registry name so two npm-aliases of the same
            // real package share one store index entry instead of
            // wastefully double-fetching under the alias. Integrity
            // is part of the cache key so a different tarball served
            // under the same (name, version) — e.g. a github codeload
            // archive vs. the npm-published bytes — can't return the
            // wrong file list.
            //
            // Stat depth follows the warm-store-verify seam. By default
            // (upstream) this is a full per-file verify: the index cache
            // and the CAS shards live in separate paths until the
            // v1/index/ migration completes on disk, and external systems
            // can drift them apart even after (Docker BuildKit cache
            // mounts that only cover one path, foreign sync tools, partial
            // wipes mid-install). Under an embedder that opted into
            // fast-trust via `warm_store_verify = false` on the embedder profile, only the
            // first file per package is stat'd — enough to catch the
            // common wiped-CAS-shard crash residue. Either way a stale
            // index drops here and falls through to `NeedsFetch`, which
            // re-fetches the tarball cleanly — the alternative is the
            // materializer dying mid-link with
            // `ERR_AUBE_MISSING_STORE_FILE`, forcing a whole-install
            // retry. Independent of import-time SRI / `verifyStoreIntegrity`,
            // which is enforced on fetch regardless of this flag.
            match super::warm_load_index(
                store,
                pkg.registry_name(),
                &pkg.version,
                pkg.integrity.as_deref(),
            ) {
                Some(index) => (dep_path.clone(), pkg, CheckResult::Cached(index)),
                None => (dep_path.clone(), pkg, CheckResult::NeedsFetch),
            }
        })
        .collect();

    let mut indices: BTreeMap<String, aube_store::PackageIndex> = BTreeMap::new();

    // Remote tarball deps need a registry client to download the
    // bits during `import_local_source`. Build it eagerly when any
    // package has a RemoteTarball source so the local-import loop
    // can share a single reqwest client with the fetch branch
    // below. Projects without URL tarballs still get the lazy
    // construction path in the `to_fetch` branch.
    let has_remote_tarball = packages.values().any(|p| {
        matches!(
            p.local_source,
            Some(aube_lockfile::LocalSource::RemoteTarball(_))
        )
    });
    let mut client_slot: Option<std::sync::Arc<aube_registry::client::RegistryClient>> = None;
    let mut client_builder = Some(client);
    if has_remote_tarball {
        client_slot = Some((client_builder.take().unwrap())());
    }

    // Local (`file:` / `link:`) packages: import directories or
    // tarballs straight into the store so the linker has a
    // PackageIndex to walk. Link-only deps don't get an index.
    for (dep_path, pkg) in packages {
        let Some(ref local) = pkg.local_source else {
            continue;
        };
        // Credit every local dep against the overall progress total —
        // the total was seeded with `graph.packages.len()`, which
        // includes `link:` packages even though they have no
        // store-backed index. Skipping the `inc` for `None` would
        // stall the bar below 100% for any project with a link dep.
        if let Some(index) = import_local_source(
            store,
            project_root,
            local,
            client_slot.as_ref(),
            ignore_scripts,
            git_prepare_depth,
            inherited_build_policy.clone(),
            &git_shallow_hosts,
            &pkg.name,
            &pkg.version,
        )
        .await?
        {
            indices.insert(dep_path.clone(), index);
        }
        if let Some(p) = progress {
            p.inc_reused(1);
        }
    }

    // Cap by check_results upper bound. Worst case fits in one alloc.
    let mut to_fetch = Vec::with_capacity(check_results.len());
    let mut cached_count = 0usize;

    for (dep_path, pkg, result) in check_results {
        match result {
            CheckResult::AlreadyLinked => {
                // No `indices` entry: the linker takes the
                // already-materialized fast path and never touches the
                // index map for this dep_path.
                cached_count += 1;
            }
            CheckResult::Cached(index) => {
                // Don't stream Cached items through the materializer.
                // The link phase fast-paths them via pkg_nm_dir.exists()
                // anyway, so the per-pkg spawn pair was pure overhead
                // on warm-cache installs (1400-pkg fixture saw +66%
                // wall time before the skip).
                indices.insert(dep_path, index);
                cached_count += 1;
            }
            CheckResult::NeedsFetch => {
                // `registry_name` is the real package name on the
                // registry — equal to `name` for the common case, and
                // the aliased-real-name for npm-alias entries. The
                // tarball URL override is only present for aliased
                // entries where `client.tarball_url(&name, ...)` would
                // 404 the alias-qualified name; the lockfile reader
                // populated it from `resolved:` at parse time.
                to_fetch.push((
                    dep_path,
                    pkg.name.clone(),
                    pkg.registry_name().to_string(),
                    pkg.version.clone(),
                    pkg.tarball_url.clone(),
                    pkg.integrity.clone(),
                ));
            }
        }
    }

    // Credit cached packages against the overall counter immediately — only
    // the to_fetch set produces visible child rows.
    if let Some(p) = progress {
        p.inc_reused(cached_count);
    }

    // Critical-path fetch order: float likely-native-build packages
    // (sharp, esbuild, @swc/*, sqlite3, lmdb, bcrypt, etc) to the
    // front of the queue. These packages run prebuild/node-gyp at
    // install time, and starting their fetch first lets the build
    // step pipeline with subsequent fetches instead of blocking on
    // the tail. `sort_by_key` is stable so non-native packages keep
    // their lockfile-discovery order; only the natives jump ahead.
    // `AUBE_DISABLE_CRITICAL_PATH=1` reverts to the previous order
    // for byte-identity comparison runs.
    if aube_util::env::embedder_env("DISABLE_CRITICAL_PATH").is_none() {
        to_fetch
            .sort_by_key(|(_, _, registry_name, _, _, _)| !is_likely_native_build(registry_name));
    }
    let fetch_count = to_fetch.len();

    let mut lockfile_persist_handle: Option<(
        std::sync::Arc<aube_util::adaptive::PersistentState>,
        std::sync::Arc<aube_util::adaptive::AdaptiveLimit>,
    )> = None;

    if !to_fetch.is_empty() {
        // Only build the reqwest+TLS client now that we know we
        // actually need to fetch tarballs. On a warm no-op install
        // everything classifies as `AlreadyLinked` / `Cached` and this
        // closure is never called — the previous eager construction
        // cost ~22 ms on Linux just to create a client that never
        // sent a single request.
        let client = match client_slot.take() {
            Some(c) => c,
            None => (client_builder.take().unwrap())(),
        };
        /*
         * Adaptive concurrency on the lockfile driven fetch path
         * (frozen / fetch / ci / matched lockfile). Same gradient
         * controller as the streaming resolver fetch path.
         * `networkConcurrency` setting acts as the seed when set.
         * Cross run persisted under `tarball:default` so this path
         * shares its converged operating point with the streaming
         * tarball path.
         */
        let sem_seed = network_concurrency.unwrap_or_else(default_lockfile_network_concurrency);
        let lockfile_persistent = aube_util::adaptive::global_persistent_state();
        let semaphore = match lockfile_persistent.as_ref() {
            Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
                state,
                "tarball:default",
                sem_seed.clamp(64, 128),
                4,
                256,
            ),
            None => aube_util::adaptive::AdaptiveLimit::new(sem_seed.clamp(64, 128), 4, 256),
        };
        if let Some(state) = lockfile_persistent.clone() {
            lockfile_persist_handle = Some((state, std::sync::Arc::clone(&semaphore)));
        }
        // Hoist env-driven flags out of the per-tarball closure so
        // the libc lock fires once instead of N times on a 1000-pkg
        // install.
        let streaming_sha512_enabled =
            aube_util::env::embedder_env("DISABLE_STREAMING_SHA512").is_none();
        let tarball_stream_enabled =
            aube_util::env::embedder_env("DISABLE_TARBALL_STREAM").is_none();
        // JoinSet so a first-error path aborts the sibling fetches
        // instead of detaching them into the background. Detached
        // tasks keep writing to the CAS after the install command
        // has already errored out.
        let mut handles: tokio::task::JoinSet<miette::Result<(String, aube_store::PackageIndex)>> =
            tokio::task::JoinSet::new();

        for (dep_path, display_name, registry_name, version, tarball_url_override, integrity) in
            to_fetch
        {
            let sem = semaphore.clone();
            let store = store.clone();
            let client = client.clone();
            let row = progress.map(|p| p.start_fetch(&display_name, &version));
            let bytes_progress = progress.cloned();

            handles.spawn(async move {
                let _row = row;
                let task_start = std::time::Instant::now();
                let permit = sem.acquire().await;
                let wait_time = task_start.elapsed();
                // Aliased entries (`"h3-v2": "npm:h3@..."`) carry the
                // resolved tarball URL verbatim from the lockfile so
                // we skip re-deriving it from `registry_name` — the
                // lockfile captured the exact URL at write time
                // against whatever registry was active then.
                let url = tarball_url_override
                    .clone()
                    .unwrap_or_else(|| client.tarball_url(&registry_name, &version));
                if let Some(lockfile_url) = tarball_url_override.as_deref() {
                    verify_lockfile_tarball_url(&client, &registry_name, &version, lockfile_url)
                        .await?;
                }

                let dl_start = std::time::Instant::now();

                // Stream when env enabled and SRI is SHA-512 (or
                // absent). Streaming verify can't re-hash with
                // another algo, so non-SHA-512 takes the buffered
                // path below.
                let stream_eligible = tarball_stream_enabled
                    && integrity
                        .as_deref()
                        .is_none_or(|s| s.starts_with("sha512-"));
                if stream_eligible {
                    let streamed = crate::commands::install::lifecycle::fetch_and_import_tarball_streaming(
                        &client,
                        &store,
                        &url,
                        &display_name,
                        &registry_name,
                        &version,
                        integrity.as_deref(),
                        verify_integrity,
                        strict_integrity,
                        strict_pkg_content_check,
                    )
                    .await;
                    let (index, bytes_len) = match streamed {
                        Ok(v) => {
                            permit.record_success();
                            v
                        }
                        Err(e) => {
                            if e.is_throttle {
                                permit.record_throttle();
                            } else {
                                permit.record_cancelled();
                            }
                            return Err(e.into());
                        }
                    };
                    let dl_time = dl_start.elapsed();
                    if let Some(p) = bytes_progress.as_ref() {
                        p.inc_downloaded_bytes(bytes_len);
                    }
                    tracing::trace!(
                        "fetch (stream) {display_name}@{version}: wait={:.0?} total={:.0?} ({} bytes)",
                        wait_time,
                        dl_time,
                        bytes_len
                    );
                    return Ok::<_, miette::Report>((dep_path, index));
                }

                // Buffered SHA-512 path. Streaming SHA-512 hashes
                // chunks during the read loop, so import_verified
                // skips its hash pass and compares directly.
                // AUBE_DISABLE_STREAMING_SHA512=1 reverts to the
                // buffered-then-hash path.
                let fetch_outcome = if streaming_sha512_enabled {
                    client
                        .fetch_tarball_bytes_streaming_sha512(&url)
                        .await
                        .map(|(b, d)| (b, Some(d)))
                        .map_err(|e| {
                            let throttled = e.is_throttle();
                            (
                                miette!(
                                    "failed to fetch {display_name}@{version}: {e}{}",
                                    crate::dep_chain::format_chain_for(&display_name, &version)
                                ),
                                throttled,
                            )
                        })
                } else {
                    client.fetch_tarball_bytes(&url).await.map(|b| (b, None)).map_err(|e| {
                        let throttled = e.is_throttle();
                        (
                            miette!(
                                "failed to fetch {display_name}@{version}: {e}{}",
                                crate::dep_chain::format_chain_for(&display_name, &version)
                            ),
                            throttled,
                        )
                    })
                };
                let (bytes, streamed_digest) = match fetch_outcome {
                    Ok(v) => {
                        permit.record_success();
                        v
                    }
                    Err((report, throttled)) => {
                        if throttled {
                            permit.record_throttle();
                        } else {
                            permit.record_cancelled();
                        }
                        return Err(report);
                    }
                };
                let dl_time = dl_start.elapsed();

                if let Some(p) = bytes_progress.as_ref() {
                    p.inc_downloaded_bytes(bytes.len() as u64);
                }

                // Keep the semaphore permit through import, not just
                // download. `import_tarball` fans out into gzip/tar
                // decode, SHA-512, CAS writes, and index writes; on
                // macOS/APFS, letting hundreds of completed downloads
                // pile into Tokio's large blocking pool turns the
                // cold-cache path into metadata contention. The
                // semaphore is therefore the install-wide "download +
                // import" pressure valve: enough concurrency to keep
                // the network busy, but not enough to swamp the
                // filesystem.
                //
                // Move CPU/blocking work (SHA-512 verify, tar extract,
                // file writes, index cache write) onto the blocking
                // thread pool so it doesn't starve the async runtime
                // workers used for concurrent network I/O.
                let bytes_len = bytes.len();
                let (index, import_time) = run_import_on_blocking(
                    store.clone(),
                    bytes,
                    streamed_digest,
                    display_name.clone(),
                    registry_name.clone(),
                    version.clone(),
                    integrity.clone(),
                    verify_integrity,
                    strict_integrity,
                    strict_pkg_content_check,
                )
                .await?;

                tracing::trace!(
                    "fetch {display_name}@{version}: wait={:.0?} dl={:.0?} ({} bytes) import={:.0?}",
                    wait_time,
                    dl_time,
                    bytes_len,
                    import_time
                );

                Ok::<_, miette::Report>((dep_path, index))
            });
        }

        while let Some(joined) = handles.join_next().await {
            let (dep_path, index) = joined.into_diagnostic()??;
            if let Some(tx) = materialize_tx.as_ref() {
                // Time channel send so back-pressure events show up in
                // the trace. The materialize channel is bounded; if the
                // consumer falls behind, `send().await` blocks until a
                // permit frees, which is otherwise invisible in
                // `fetch.tarball` totals.
                let send_t0 = aube_util::diag::enabled().then(std::time::Instant::now);
                tx.send((dep_path.clone(), index.clone()))
                    .await
                    .map_err(|_| {
                        miette!("materializer task exited before fetch_packages finished")
                    })?;
                if let Some(t0) = send_t0 {
                    let elapsed = t0.elapsed();
                    if elapsed.as_millis() >= 1 {
                        aube_util::diag::event(
                            aube_util::diag::Category::Channel,
                            "materialize_send_wait",
                            elapsed,
                            None,
                        );
                    }
                }
            }
            indices.insert(dep_path, index);
        }
    }

    // Without explicit drop, consumer's rx.recv() loop hangs.
    drop(materialize_tx);

    if let Some((state, sem)) = lockfile_persist_handle {
        sem.persist(&state, "tarball:default");
    }

    Ok((indices, cached_count, fetch_count))
}

async fn verify_lockfile_tarball_url(
    client: &aube_registry::client::RegistryClient,
    registry_name: &str,
    version: &str,
    lockfile_url: &str,
) -> miette::Result<()> {
    let packument = client
        .fetch_packument_cached(registry_name, &packument_cache_dir())
        .await
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_TARBALL_URL_MISMATCH,
                "{}@{}: failed to fetch registry metadata to verify lockfile tarball URL: {}",
                registry_name,
                version,
                e
            )
        })?;
    let Some(meta) = packument.versions.get(version) else {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_TARBALL_URL_MISMATCH,
            "{}@{}: registry metadata did not include this version while lockfile pinned {}",
            registry_name,
            version,
            aube_util::url::redact_url(lockfile_url)
        ));
    };
    let Some(expected_url) = meta.dist.as_ref().map(|dist| dist.tarball.as_str()) else {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_TARBALL_URL_MISMATCH,
            "{}@{}: registry metadata did not include dist.tarball while lockfile pinned {}",
            registry_name,
            version,
            aube_util::url::redact_url(lockfile_url)
        ));
    };
    if !lockfile_tarball_url_matches_metadata(lockfile_url, expected_url) {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_TARBALL_URL_MISMATCH,
            "{}@{}: lockfile tarball URL {} does not match registry metadata {}",
            registry_name,
            version,
            aube_util::url::redact_url(lockfile_url),
            aube_util::url::redact_url(expected_url)
        ));
    }
    Ok(())
}

fn lockfile_tarball_url_matches_metadata(lockfile_url: &str, expected_url: &str) -> bool {
    lockfile_url == expected_url
        || (is_public_npm_registry_tarball(lockfile_url)
            && tarball_url_path(lockfile_url)
                .zip(tarball_url_path(expected_url))
                .is_some_and(|(lockfile_path, expected_path)| lockfile_path == expected_path))
}

fn is_public_npm_registry_tarball(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .as_deref()
        == Some("registry.npmjs.org")
}

fn tarball_url_path(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let path = url.path();
    path.contains("/-/")
        .then_some(path.trim_start_matches('/').to_string())
}

/// Pull the canonical version off a dep_path for display purposes. The
/// dep_path looks like `name@1.2.3(peer@x)` — we strip the `name@` prefix
/// and any peer suffix so the warning shows `1.2.3` not `1.2.3(peer@x)`.
pub(super) fn version_from_dep_path(dep_path: &str, name: &str) -> String {
    let tail = dep_path
        .strip_prefix(&format!("{name}@"))
        .unwrap_or(dep_path);
    tail.split('(').next().unwrap_or(tail).to_string()
}

/// Re-key a canonical-indexed indices map to match the peer-contextualized
/// dep_paths in `graph`. Each contextualized entry points at the same
/// underlying files as its canonical name@version, so we look each graph
/// entry up by canonical and clone the index — a no-op when canonical ==
/// contextualized (i.e. the package has no peer deps).
pub(super) fn remap_indices_to_contextualized(
    canonical_indices: &BTreeMap<String, aube_store::PackageIndex>,
    graph: &aube_lockfile::LockfileGraph,
) -> BTreeMap<String, aube_store::PackageIndex> {
    let mut out = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        let canonical_key = pkg.spec_key();
        // The peer-context pass appends a `(peer@ver)` suffix (or a
        // parenthesized `(<short-hash>)` when it exceeds the cap) onto a
        // package's canonical dep_path. Source-backed deps (git /
        // remote tarball / file) are streamed from the resolver — and
        // therefore keyed in `canonical_indices` — under their
        // *source-coordinate* dep_path (`name@git+<short>`), not their
        // semver `spec_key()`. So once such a dep picks up a peer
        // suffix, neither the contextualized `dep_path` (carries the
        // suffix) nor `spec_key()` (semver, not the git coordinate)
        // matches the streamed key, and the index would be silently
        // dropped — later tripping `ERR_AUBE_MISSING_PACKAGE_INDEX` in
        // the linker's global-virtual-store pass. Stripping the suffix
        // recovers the exact canonical coordinate the index was stored
        // under (the peer-context pass builds the key as
        // `{canonical_base}{suffix}`, so this is its precise inverse).
        let canonical_dep_path = strip_peer_context_suffix(dep_path);
        if let Some(idx) = canonical_indices
            .get(dep_path)
            .or_else(|| canonical_indices.get(canonical_dep_path))
            .or_else(|| canonical_indices.get(&canonical_key))
        {
            out.insert(dep_path.clone(), idx.clone());
        }
    }
    out
}

/// Strip the peer-context suffix from a `dep_path`, recovering the
/// canonical dep_path the resolver streamed it under (and that
/// `canonical_indices` is keyed by). The peer-context pass in
/// `aube-resolver` appends either a parenthesized `(peer@ver)…` tail
/// or, when the suffix body exceeds the length cap, a single
/// parenthesized short hash `(<short-hash>)` (pnpm's
/// `createPeerDepGraphHash`). Both forms begin at the first `(`, so
/// cutting there is the exact inverse and recovers the canonical
/// coordinate. A `dep_path` with no suffix is returned unchanged — a
/// bare `_<hex>` tail belongs to a `git+`/`url+`/`file+` source
/// coordinate and is never a peer marker, so it is preserved.
fn strip_peer_context_suffix(dep_path: &str) -> &str {
    dep_path.split('(').next().unwrap_or(dep_path)
}

#[cfg(test)]
mod tests {
    use super::lockfile_tarball_url_matches_metadata;

    #[test]
    fn tarball_url_match_accepts_exact_url() {
        let url = "https://private.example.com/is-odd/-/is-odd-3.0.1.tgz";

        assert!(lockfile_tarball_url_matches_metadata(url, url));
    }

    #[test]
    fn tarball_url_match_accepts_public_npm_lockfile_against_mirror() {
        assert!(lockfile_tarball_url_matches_metadata(
            "https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz",
            "http://localhost:4873/is-odd/-/is-odd-3.0.1.tgz",
        ));
    }

    #[test]
    fn tarball_url_match_accepts_scoped_public_npm_lockfile_against_mirror() {
        assert!(lockfile_tarball_url_matches_metadata(
            "https://registry.npmjs.org/@isaacs/fs-minipass/-/fs-minipass-4.0.1.tgz",
            "http://localhost:4873/@isaacs/fs-minipass/-/fs-minipass-4.0.1.tgz",
        ));
    }

    #[test]
    fn tarball_url_match_rejects_tampered_path() {
        assert!(!lockfile_tarball_url_matches_metadata(
            "https://registry.npmjs.org/not-is-odd/-/not-is-odd-3.0.1.tgz",
            "http://localhost:4873/is-odd/-/is-odd-3.0.1.tgz",
        ));
    }

    #[test]
    fn tarball_url_match_rejects_mirror_match_from_arbitrary_host() {
        assert!(!lockfile_tarball_url_matches_metadata(
            "https://example.com/is-odd/-/is-odd-3.0.1.tgz",
            "http://localhost:4873/is-odd/-/is-odd-3.0.1.tgz",
        ));
    }

    #[test]
    fn strip_peer_context_suffix_recovers_canonical_base() {
        use super::strip_peer_context_suffix;
        // Plain parenthesized peer suffix.
        assert_eq!(
            strip_peer_context_suffix("a@git+abc1234567890123(react@18.0.0)"),
            "a@git+abc1234567890123"
        );
        // Multiple / nested peer segments.
        assert_eq!(
            strip_peer_context_suffix("a@git+abc1234567890123(b@1.0.0)(c@2.0.0)"),
            "a@git+abc1234567890123"
        );
        // Hashed suffix `(<short-hash>)` emitted past the length cap.
        assert_eq!(
            strip_peer_context_suffix("a@git+abc1234567890123(0123456789abcdef0123456789abcdef)"),
            "a@git+abc1234567890123"
        );
        // Scoped name keeps its leading `@scope/` intact.
        assert_eq!(
            strip_peer_context_suffix("@scope/a@git+abc1234567890123(b@1)"),
            "@scope/a@git+abc1234567890123"
        );
        // No suffix → unchanged.
        assert_eq!(strip_peer_context_suffix("a@1.0.0"), "a@1.0.0");
        // A bare `_<hex>` tail belongs to the source coordinate, not a
        // peer marker — only parenthesized suffixes are stripped, so it
        // is preserved (the old bare-marker stripper would have wrongly
        // truncated it).
        assert_eq!(
            strip_peer_context_suffix("a@git+abc1234567890123_0123456789"),
            "a@git+abc1234567890123_0123456789"
        );
    }

    fn one_file_index() -> aube_store::PackageIndex {
        let mut index = aube_store::PackageIndex::default();
        index.insert(
            "package.json".to_string(),
            aube_store::StoredFile {
                hex_hash: "deadbeef".to_string(),
                store_path: std::path::PathBuf::from("/store/de/adbeef"),
                executable: false,
                size: Some(2),
            },
        );
        index
    }

    fn git_pkg(dep_path: &str) -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: "left-pad".to_string(),
            version: "5.0.0".to_string(),
            dep_path: dep_path.to_string(),
            local_source: Some(aube_lockfile::LocalSource::Git(aube_lockfile::GitSource {
                url: "git+https://github.com/x/left-pad.git".to_string(),
                committish: None,
                resolved: "3c803342e33ad8281122334455667788".to_string(),
                integrity: None,
                subpath: None,
            })),
            ..Default::default()
        }
    }

    // Regression: a git dep that the peer-context pass tags with a
    // `(peer@ver)` suffix must still resolve its streamed canonical
    // index. Before the fix the contextualized dep_path missed both
    // lookups (`dep_path` carries the suffix; `spec_key()` is the
    // semver, not the git coordinate), so the linker's global-virtual-
    // store pass tripped `ERR_AUBE_MISSING_PACKAGE_INDEX`.
    #[test]
    fn remap_recovers_git_dep_index_through_peer_suffix() {
        let canonical = "left-pad@git+3c803342e33ad828";
        let contextualized = format!("{canonical}(ramda@0.30.1)");

        let mut canonical_indices = std::collections::BTreeMap::new();
        canonical_indices.insert(canonical.to_string(), one_file_index());

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert(contextualized.clone(), git_pkg(&contextualized));

        let out = super::remap_indices_to_contextualized(&canonical_indices, &graph);
        assert!(
            out.contains_key(&contextualized),
            "git dep with peer suffix should recover its canonical index; got {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    // Same recovery for the hashed-suffix form the peer-context pass
    // emits when the suffix body exceeds the length cap: a single
    // parenthesized short hash `(<short-hash>)`.
    #[test]
    fn remap_recovers_git_dep_index_through_hashed_peer_suffix() {
        let canonical = "left-pad@git+3c803342e33ad828";
        let contextualized = format!("{canonical}(0123456789abcdef0123456789abcdef)");

        let mut canonical_indices = std::collections::BTreeMap::new();
        canonical_indices.insert(canonical.to_string(), one_file_index());

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert(contextualized.clone(), git_pkg(&contextualized));

        let out = super::remap_indices_to_contextualized(&canonical_indices, &graph);
        assert!(
            out.contains_key(&contextualized),
            "git dep with hashed peer suffix should recover its canonical index; got {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    // A suffix-less git dep keyed identically in both maps still maps
    // straight through (guards against the strip changing the happy path).
    #[test]
    fn remap_maps_suffixless_git_dep_directly() {
        let canonical = "left-pad@git+3c803342e33ad828";

        let mut canonical_indices = std::collections::BTreeMap::new();
        canonical_indices.insert(canonical.to_string(), one_file_index());

        let mut graph = aube_lockfile::LockfileGraph::default();
        graph
            .packages
            .insert(canonical.to_string(), git_pkg(canonical));

        let out = super::remap_indices_to_contextualized(&canonical_indices, &graph);
        assert!(out.contains_key(canonical));
    }
}
