//! Shared helpers for the `patch` / `patch-commit` / `patch-remove`
//! commands and the install-time patch application path.
//!
//! Patches are stored alongside the project (default `patches/`) and
//! tracked as `{ "name@version": "patches/name@version.patch" }`.
//! Sources are merged from Bun's top-level `patchedDependencies`,
//! `pnpm.patchedDependencies` / `aube.patchedDependencies`, then
//! workspace-yaml `patchedDependencies`, in that precedence order.

use miette::{Context, IntoDiagnostic, Result, miette};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One resolved patch entry. `key` is the VERBATIM declared
/// `patchedDependencies` key — exact (`ms@2.1.3`), range (`ms@>=2`),
/// wildcard (`ms@*`), or bare name (`ms`) — and is the string the
/// lockfile's `patchedDependencies:` block records unresolved, matching
/// pnpm. `path` is the absolute path on disk, `content` is the raw
/// patch text the linker applies. Mapping a concrete resolved
/// `name@version` to the entry that applies is
/// [`aube_lockfile::patch_groups`]'s job.
#[derive(Debug, Clone)]
pub struct ResolvedPatch {
    pub key: String,
    #[allow(dead_code)]
    pub path: PathBuf,
    /// The project-relative patch path exactly as declared
    /// (`patches/ms@2.1.3.patch`, forward slashes) — the string the
    /// lockfile's `patchedDependencies:` block records.
    pub rel: String,
    pub content: String,
}

impl ResolvedPatch {
    /// sha256 hex digest of the patch content, computed exactly the way
    /// pnpm computes the `patchedDependencies` lockfile value
    /// (`createHexHashFromFile`): the file is decoded as UTF-8 (lossy —
    /// done at read time) and CRLF is normalized to LF before hashing,
    /// so a patch authored on Windows hashes the same as on POSIX.
    /// Folded into the graph hash (so a patched node lives at a distinct
    /// virtual-store path) and written as the lockfile patch value, so
    /// it must agree with pnpm byte-for-byte for `--frozen-lockfile`.
    pub fn content_hash(&self) -> String {
        let normalized = self.content.replace("\r\n", "\n");
        let mut h = Sha256::new();
        h.update(normalized.as_bytes());
        hex::encode(h.finalize())
    }
}

/// True when `rel` is a project-relative patch path that stays within
/// the project root. Refuses absolute paths, Windows drive or UNC
/// prefixes, NUL bytes, and any `..` component. Used as a read-side
/// guard so a hostile manifest cannot point the patch loader at
/// arbitrary files (e.g. `/etc/passwd` or `\\server\share\secret`).
fn is_safe_patch_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute() || p.has_root() {
        return false;
    }
    // Reject a leading drive letter (`C:foo`) that `is_absolute` does
    // not always catch on the non-Windows host that rendered the
    // lockfile.
    if rel.len() >= 2 && rel.as_bytes()[1] == b':' {
        return false;
    }
    p.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

/// Validate a `patchedDependencies` key's shape, rejecting only a
/// non-`*` selector that isn't a valid semver range (pnpm's
/// `PATCH_NON_SEMVER_RANGE`). Every other shape — exact, range, `*`,
/// bare name — is accepted; mapping the key to concrete resolved
/// versions is [`aube_lockfile::patch_groups`]'s job. Kept a thin
/// wrapper so the invalid-range error carries the branded code.
fn validate_patch_key(key: &str) -> Result<()> {
    aube_lockfile::patch_groups::classify_patch_key(key).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_PATCH_NON_SEMVER_RANGE,
            "{}",
            e.message()
        )
    })?;
    Ok(())
}

/// Build the two shapes the linker + GVS-prewarm materializer want,
/// keyed by the CONCRETE resolved `name@version` those stages apply
/// patches by: a `(name@version, content)` content map and a
/// `(name@version, content_hash)` fold map. The declared keys may be
/// exact (`ms@2.1.3`), a range (`ms@>=2`), a wildcard (`ms@*`), or a
/// bare name (`ms`); each graph package resolves to at most one patch
/// by pnpm's `getPatchInfo` priority (exact > range > all). An
/// all-exact project resolves each key to its own `name@version`, so
/// the maps are byte-identical to the pre-resolution behavior.
///
/// Two ranges matching one version → [`aube_codes::errors::ERR_AUBE_PATCH_KEY_CONFLICT`];
/// an invalid non-`*` range → [`aube_codes::errors::ERR_AUBE_PATCH_NON_SEMVER_RANGE`].
pub fn load_patches_for_linker(
    cwd: &Path,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<(aube_linker::Patches, BTreeMap<String, String>)> {
    let resolved = load_resolved_patches_cached(cwd, &graph.patched_dependencies)?;
    resolve_patches_against_graph(&resolved, graph)
}

/// Cache the DISK read of the declared patch set (source key →
/// [`ResolvedPatch`]) per cwd for the process lifetime. The 2-3 link /
/// materialize / prewarm call sites within one install hit the cache on
/// calls 2+ instead of re-walking `patches/` from disk. Resolving the
/// cached set against the graph is cheap and stays out of the cache, so
/// the cache is correctly graph-independent (the same cwd is only ever
/// queried with one graph per install process anyway).
fn load_resolved_patches_cached(
    cwd: &Path,
    lockfile_patched_dependencies: &BTreeMap<String, String>,
) -> Result<std::sync::Arc<BTreeMap<String, ResolvedPatch>>> {
    use std::sync::{Arc, Mutex, OnceLock};
    type CacheKey = (PathBuf, BTreeMap<String, String>);
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<CacheKey, Arc<BTreeMap<String, ResolvedPatch>>>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    // Canonicalize so `cwd` and `cwd.canonicalize()` collapse to one
    // key. Windows `\\?\C:\foo` vs `C:\foo`, Unix `cwd` vs `cwd/.`
    // would otherwise miss the cache. Falls back to the raw path when
    // canonicalize fails (e.g. cwd is the parent of a not-yet-created
    // workspace dir).
    let key = (
        cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
        lockfile_patched_dependencies.clone(),
    );
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(&key)
    {
        return Ok(hit.clone());
    }
    let resolved = Arc::new(load_patches_with_lockfile_entries(
        cwd,
        lockfile_patched_dependencies,
    )?);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, resolved.clone());
    }
    Ok(resolved)
}

/// Map every resolved graph package to the declared patch that applies
/// (pnpm's `getPatchInfo` priority), keying the linker's content map
/// and the graph-hash fold map by the concrete `name@version` those
/// stages look up. No `local_source` filter — the linker applies to any
/// `spec_key` match, matching the pre-resolution unfiltered behavior.
fn resolve_patches_against_graph(
    resolved: &BTreeMap<String, ResolvedPatch>,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<(aube_linker::Patches, BTreeMap<String, String>)> {
    let by_version =
        aube_lockfile::patch_groups::resolve_patched_by_version(resolved, &graph.packages)
            .map_err(map_patch_resolve_err)?;
    let mut patches = aube_linker::Patches::new();
    let mut hashes = BTreeMap::new();
    for (spec, patch) in by_version {
        hashes.insert(spec.clone(), patch.content_hash());
        patches.insert(spec, patch.content);
    }
    Ok((patches, hashes))
}

/// Brand a patch-group resolution failure with the matching
/// `ERR_AUBE_PATCH_*` code (rewritten to `ERR_NUB_*` by the embedder).
fn map_patch_resolve_err(e: aube_lockfile::patch_groups::PatchResolveError) -> miette::Report {
    use aube_lockfile::patch_groups::PatchResolveError;
    match e {
        PatchResolveError::InvalidRange(r) => miette!(
            code = aube_codes::errors::ERR_AUBE_PATCH_NON_SEMVER_RANGE,
            "{}",
            r.message()
        ),
        PatchResolveError::Conflict(c) => miette!(
            code = aube_codes::errors::ERR_AUBE_PATCH_KEY_CONFLICT,
            help = c.hint(),
            "{}",
            c.message()
        ),
    }
}

#[cfg(test)]
fn load_patches(cwd: &Path) -> Result<BTreeMap<String, ResolvedPatch>> {
    load_patches_with_lockfile_entries(cwd, &BTreeMap::new())
}

fn load_patches_with_lockfile_entries(
    cwd: &Path,
    lockfile_patched_dependencies: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ResolvedPatch>> {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    entries.extend(lockfile_patched_dependencies.clone());

    let manifest_path = cwd.join("package.json");
    if manifest_path.exists() {
        let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
            .map_err(miette::Report::new)
            .wrap_err("failed to read package.json")?;
        entries.extend(manifest.bun_patched_dependencies());
        entries.extend(manifest.pnpm_patched_dependencies());
    }

    let ws_config = aube_manifest::workspace::WorkspaceConfig::load(cwd)
        .map_err(miette::Report::new)
        .wrap_err("failed to read pnpm-workspace.yaml")?;
    entries.extend(ws_config.patched_dependencies);

    let mut out = BTreeMap::new();
    for (key, rel) in entries {
        validate_patch_key(&key)?;
        // Refuse absolute paths and `..` traversal in the manifest-
        // declared patch path so a hostile `package.json` cannot
        // coerce `aube install` into reading an arbitrary file off
        // disk. The linker already guards the *apply* side with
        // `is_safe_rel_component`, and mirroring the same check on
        // the *read* side keeps the trust boundary uniform.
        if !is_safe_patch_rel(&rel) {
            return Err(miette!(
                "refusing unsafe patch path for {key}: {rel:?} (absolute, UNC, or contains `..`)"
            ));
        }
        let path = cwd.join(&rel);
        // Read raw bytes and decode lossily, matching pnpm/Node's
        // `fs.readFile(path, 'utf8')`: a patch file with stray non-UTF-8
        // bytes works under pnpm (replaced with U+FFFD) rather than
        // erroring, and the patch *hash* must agree with pnpm's
        // `createHexHashFromFile` for `--frozen-lockfile` parity.
        let raw = std::fs::read(&path).into_diagnostic().map_err(|e| {
            miette!(
                "failed to read patch file {} for {key}: {e}",
                path.display()
            )
        })?;
        let content = String::from_utf8_lossy(&raw).into_owned();
        out.insert(
            key.clone(),
            ResolvedPatch {
                key,
                path,
                rel,
                content,
            },
        );
    }
    Ok(out)
}

/// Add or replace an entry in `patchedDependencies`. The entry goes
/// where the package manager that owns the project's lockfile reads
/// it from: a bun-format project gets `package.json`'s top-level
/// `patchedDependencies` (the only location real bun consults — an
/// entry under `pnpm.patchedDependencies` would leave a frozen
/// `bun install` linking unpatched content). Everything else routes
/// through the shared
/// [`aube_manifest::workspace::config_write_target`] rule: workspace
/// yaml when one is present, otherwise `package.json`'s
/// `pnpm`/`aube` namespace. Returns the path that was rewritten so
/// the caller can report it to the user.
pub fn upsert_patched_dependency(cwd: &Path, key: &str, rel_patch_path: &str) -> Result<PathBuf> {
    use aube_manifest::workspace::ConfigWriteTarget;
    // Interop routing by the project's lockfile format. bun reads only
    // the top-level `patchedDependencies`; pnpm reads only
    // `pnpm.patchedDependencies` (or the workspace yaml, which
    // `config_write_target` already prefers when present) — an entry
    // under the `aube` namespace would make the real PM reject the
    // very lockfile we write for it (pnpm:
    // ERR_PNPM_LOCKFILE_CONFIG_MISMATCH) or silently link unpatched
    // content (bun). aube-native projects keep the configured
    // namespace rule below.
    //
    // Brand-boundary exception (the embedder symmetric boundary): when the
    // engine does NOT consume pnpm-branded config (`read_branded_pnpm_config
    // == false` — an embedder running as the project's own identity, e.g.
    // `nub` under a nub-identity project), writing the entry under the
    // `pnpm` namespace would record it where the engine then *ignores* it:
    // `pnpm_patched_dependencies()` skips the `pnpm` namespace under that
    // posture, so the patch never reaches `effective_patch_config` →
    // never lands in the lockfile → the next `--frozen-lockfile` install
    // fails with ERR_*_OUTDATED_LOCKFILE (the patch is "declared but
    // missing"). Land it in the un-branded top-level `patchedDependencies`
    // instead — the location the engine reads under its own identity.
    let reads_branded_pnpm = aube_util::engine_context().read_branded_pnpm_config;
    match aube_lockfile::detect_existing_lockfile_kind(cwd) {
        Some(aube_lockfile::LockfileKind::Bun) => {
            upsert_manifest_patched_dependency(cwd, key, rel_patch_path, None)
                .wrap_err("failed to write package.json")?;
            return Ok(cwd.join("package.json"));
        }
        Some(aube_lockfile::LockfileKind::Pnpm)
            if aube_manifest::workspace::workspace_yaml_existing(cwd).is_none() =>
        {
            // Standalone aube (reads pnpm config): nest under `pnpm` so real
            // pnpm accepts the lockfile. An embedder that ignores pnpm config:
            // write the un-branded top-level field it actually reads.
            let namespace = if reads_branded_pnpm { Some("pnpm") } else { None };
            upsert_manifest_patched_dependency(cwd, key, rel_patch_path, namespace)
                .wrap_err("failed to write package.json")?;
            return Ok(cwd.join("package.json"));
        }
        _ => {}
    }
    match aube_manifest::workspace::config_write_target(cwd) {
        ConfigWriteTarget::PackageJson => {
            aube_manifest::workspace::edit_setting_map(cwd, "patchedDependencies", |map| {
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(rel_patch_path.to_string()),
                );
            })
            .map_err(miette::Report::new)
            .wrap_err("failed to write package.json")?;
            Ok(cwd.join("package.json"))
        }
        ConfigWriteTarget::WorkspaceYaml(path) => {
            aube_manifest::workspace::upsert_workspace_patched_dependency(
                &path,
                key,
                rel_patch_path,
            )
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
            Ok(path)
        }
    }
}

/// The manifest/workspace-declared patch config — `(selector → rel
/// path, selector → sha256 hex of the patch file's current contents)`
/// — for install's lockfile drift check
/// ([`aube_lockfile::LockfileGraph::check_patched_dependencies_drift`]).
/// Deliberately excludes lockfile-carried entries: drift compares the
/// project's declared intent against what the lockfile recorded.
/// Errors on a declared-but-missing patch file, same as the linker.
pub fn effective_patch_config(
    cwd: &Path,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let resolved = load_patches_with_lockfile_entries(cwd, &BTreeMap::new())?;
    let mut paths = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for patch in resolved.values() {
        paths.insert(patch.key.clone(), patch.rel.clone());
        hashes.insert(patch.key.clone(), patch.content_hash());
    }
    Ok((paths, hashes))
}

/// Record the project's patch configuration on a freshly resolved
/// graph so the lockfile writers emit it the way the owning package
/// manager does: pnpm 10's `patchedDependencies: { hash, path }` block
/// plus `(patch_hash=…)` dep-path suffixes, bun's path-form
/// `patchedDependencies` block. The config — Bun's top-level
/// `patchedDependencies`, `pnpm.patchedDependencies` /
/// `aube.patchedDependencies`, then workspace yaml — *replaces*
/// whatever the graph carried: it is the user's intent, and keeping
/// stale lockfile-carried entries would resurrect patches the user
/// just `patch-remove`d. The hash is the sha256 hex of the patch file
/// contents — exactly what pnpm computes and verifies on a frozen
/// install.
pub fn record_patches_on_graph(cwd: &Path, graph: &mut aube_lockfile::LockfileGraph) -> Result<()> {
    let (paths, hashes) = effective_patch_config(cwd)?;
    // Surface a range CONFLICT (two ranges matching one resolved
    // version) here at resolve time — the earliest, most uniform gate,
    // matching pnpm's "raise during config grouping". Without it the
    // conflict would only surface later at link / write, and a
    // `--lockfile-only` no-op write could suppress it entirely. An
    // invalid range already errored inside `effective_patch_config`.
    aube_lockfile::patch_groups::resolve_patched_by_version(&paths, &graph.packages)
        .map_err(map_patch_resolve_err)?;
    verify_patches_used(cwd, &paths, graph)?;
    graph.patched_dependencies = paths;
    graph.patched_dependency_hashes = hashes;
    Ok(())
}

/// pnpm's `verifyPatches`: a declared `patchedDependencies` key that
/// matches no installed package fails the install, unless
/// `allowUnusedPatches` downgrades it to a warning.
///
/// Runs here, at the resolve gate, because that is where pnpm runs it —
/// inside the resolution step, which a warm "lockfile is up to date"
/// install skips entirely (verified against pnpm 10.15.1). Reporting
/// every unused key at once, rather than the first, also matches pnpm,
/// and firing before the graph is handed on keeps the failure ahead of
/// any linking or lockfile write.
fn verify_patches_used(
    cwd: &Path,
    paths: &BTreeMap<String, String>,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<()> {
    let unused = aube_lockfile::patch_groups::unused_patch_keys(
        paths.keys().map(String::as_str),
        &graph.packages,
    )
    // An invalid range is `effective_patch_config`'s error to raise, and
    // it already did; nothing here can add to it.
    .unwrap_or_default();
    if unused.is_empty() {
        return Ok(());
    }
    let keys: Vec<&str> = unused.iter().map(|u| u.source_key).collect();
    // Byte-identical to pnpm's ERR_PNPM_UNUSED_PATCH wording so a user
    // moving between the two reads the same sentence.
    let message = format!("The following patches were not used: {}", keys.join(", "));
    // An alias-named key is the one unused shape with a mechanical fix,
    // so it carries the exact replacement rather than the generic hint.
    let hint = unused
        .iter()
        .find_map(|u| {
            let spelling = u.registry_name_spelling.as_deref()?;
            Some(format!(
                "Patches resolve against a package's registry name, not the npm alias it is \
                 installed as — declare {:?} as {spelling:?}.",
                u.source_key
            ))
        })
        .unwrap_or_else(|| {
            "Either remove them from \"patchedDependencies\" or update them to match packages \
             in your dependencies."
                .to_string()
        });
    if allow_unused_patches(cwd) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_UNUSED_PATCH,
            "{message} {hint}"
        );
        return Ok(());
    }
    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_UNUSED_PATCH,
        help = hint,
        "{message}"
    ))
}

/// Resolve `allowUnusedPatches` from the two homes pnpm 10 honors it in
/// — the manifest and the workspace yaml — with the workspace yaml
/// winning, matching how `patchedDependencies` itself merges. Each
/// reader applies its own brand gating, so a nub-identity project sees
/// only neutral, un-branded config.
fn allow_unused_patches(cwd: &Path) -> bool {
    if let Ok(ws) = aube_manifest::workspace::WorkspaceConfig::load(cwd)
        && let Some(allow) = ws.allow_unused_patches
    {
        return allow;
    }
    aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
        .ok()
        .and_then(|m| m.allow_unused_patches())
        .unwrap_or(false)
}

/// Drop an entry from `patchedDependencies` in whichever file declares
/// it (workspace yaml, Bun's top-level
/// `package.json#patchedDependencies`,
/// `package.json#pnpm.patchedDependencies`,
/// `package.json#aube.patchedDependencies`, or any combination).
/// Returns the files that were rewritten — empty when no location held
/// the entry. Each side peeks before rewriting so the no-op case
/// leaves the file (and any user yaml comments) intact.
pub fn remove_patched_dependency(cwd: &Path, key: &str) -> Result<Vec<PathBuf>> {
    let mut rewritten = Vec::new();
    if let Some(ws_path) = aube_manifest::workspace::workspace_yaml_existing(cwd)
        && aube_manifest::workspace::remove_workspace_patched_dependency(&ws_path, key)
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to write {}", ws_path.display()))?
    {
        rewritten.push(ws_path);
    }
    let removed_namespaced =
        aube_manifest::workspace::remove_setting_entry(cwd, "patchedDependencies", key)
            .map_err(miette::Report::new)
            .wrap_err("failed to write package.json")?;
    let removed_bun =
        remove_bun_patched_dependency(cwd, key).wrap_err("failed to write package.json")?;
    if removed_namespaced || removed_bun {
        rewritten.push(cwd.join("package.json"));
    }
    Ok(rewritten)
}

/// Add or replace `key` in a `patchedDependencies` map in
/// `package.json` — top-level when `namespace` is `None` (bun's
/// location), nested under the named object otherwise (`pnpm` for
/// pnpm-format projects). Creates the map (and namespace object)
/// when absent.
fn upsert_manifest_patched_dependency(
    cwd: &Path,
    key: &str,
    rel_patch_path: &str,
    namespace: Option<&str>,
) -> Result<()> {
    let path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&path)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", path.display()))?;
    let mut value =
        aube_manifest::parse_json::<serde_json::Value>(&path, raw).map_err(miette::Report::new)?;
    let mut obj = value
        .as_object_mut()
        .ok_or_else(|| miette!("package.json is not an object"))?;
    if let Some(ns) = namespace {
        obj = obj
            .entry(ns)
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| miette!("package.json `{ns}` is not an object"))?;
    }
    let patched = obj
        .entry("patchedDependencies")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let patched = patched
        .as_object_mut()
        .ok_or_else(|| miette!("package.json `patchedDependencies` is not an object"))?;
    patched.insert(
        key.to_string(),
        serde_json::Value::String(rel_patch_path.to_string()),
    );
    let mut out = serde_json::to_string_pretty(&value)
        .into_diagnostic()
        .map_err(|e| miette!("failed to serialize {}: {e}", path.display()))?;
    out.push('\n');
    std::fs::write(&path, out)
        .into_diagnostic()
        .map_err(|e| miette!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

fn remove_bun_patched_dependency(cwd: &Path, key: &str) -> Result<bool> {
    let path = cwd.join("package.json");
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", path.display()))?;
    let mut value =
        aube_manifest::parse_json::<serde_json::Value>(&path, raw).map_err(miette::Report::new)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| miette!("package.json is not an object"))?;
    let before = obj.clone();

    let mut remove_empty_map = false;
    let removed = if let Some(patched) = obj
        .get_mut("patchedDependencies")
        .and_then(serde_json::Value::as_object_mut)
    {
        let removed = patched.remove(key).is_some();
        if removed {
            remove_empty_map = patched.is_empty();
        }
        removed
    } else {
        false
    };
    if remove_empty_map {
        obj.remove("patchedDependencies");
    }
    if *obj == before {
        return Ok(removed);
    }

    let mut out = serde_json::to_string_pretty(&value)
        .into_diagnostic()
        .map_err(|e| miette!("failed to serialize {}: {e}", path.display()))?;
    out.push('\n');
    std::fs::write(&path, out)
        .into_diagnostic()
        .map_err(|e| miette!("failed to write {}: {e}", path.display()))?;
    Ok(removed)
}

/// Read every declared `patchedDependencies` entry across both the
/// workspace yaml and `package.json`, with workspace-yaml entries
/// winning on key conflict (same precedence used by `load_patches`).
pub fn read_patched_dependencies(cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = read_package_json_patched_dependencies(cwd)?;
    let ws_config = aube_manifest::workspace::WorkspaceConfig::load(cwd)
        .map_err(miette::Report::new)
        .wrap_err("failed to read workspace yaml")?;
    out.extend(ws_config.patched_dependencies);
    Ok(out)
}

fn read_package_json_patched_dependencies(cwd: &Path) -> Result<BTreeMap<String, String>> {
    let manifest_path = cwd.join("package.json");
    if !manifest_path.exists() {
        return Ok(BTreeMap::new());
    }
    let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    let mut out = manifest.bun_patched_dependencies();
    out.extend(manifest.pnpm_patched_dependencies());
    Ok(out)
}

/// Recursively copy `src` into `dst`, following file content but
/// preserving relative layout. Used by `aube patch` to snapshot a
/// package out of the virtual store into both a "source" reference
/// directory and a "user edit" directory.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .into_diagnostic()
        .map_err(|e| miette!("failed to create {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", src.display()))?
    {
        let entry = entry
            .into_diagnostic()
            .map_err(|e| miette!("failed to read entry under {}: {e}", src.display()))?;
        let ty = entry
            .file_type()
            .into_diagnostic()
            .map_err(|e| miette!("failed to stat {}: {e}", entry.path().display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_symlink() {
            // Skip symlinks — packages we extract from the virtual
            // store can contain `node_modules` symlinks pointing into
            // sibling packages, which we don't want to drag into the
            // patch source dir.
            continue;
        } else {
            std::fs::copy(&from, &to).into_diagnostic().map_err(|e| {
                miette!("failed to copy {} -> {}: {e}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_all_pnpm_key_shapes() {
        // The parse-time gate now rejects ONLY a non-`*` invalid range;
        // exact, range, `*`, and bare-name keys all pass.
        for key in [
            "is-positive@3.1.0",
            "@babel/core@7.0.0",
            "sonda",
            "sonda@*",
            "sonda@>=1",
        ] {
            assert!(validate_patch_key(key).is_ok(), "should accept {key:?}");
        }
        assert!(validate_patch_key("sonda@not-a-range").is_err());
    }

    fn patch_with_content(content: &str) -> ResolvedPatch {
        ResolvedPatch {
            key: "ms@2.1.3".into(),
            path: PathBuf::from("patches/ms@2.1.3.patch"),
            rel: "patches/ms@2.1.3.patch".into(),
            content: content.into(),
        }
    }

    /// `content_hash` must equal pnpm's `createHexHashFromFile`: sha256
    /// hex of the UTF-8 text. sha256 of `"hello\n"` is the known vector
    /// from pnpm's own crypto.hash tests.
    #[test]
    fn content_hash_matches_pnpm_sha256_hex() {
        assert_eq!(
            patch_with_content("hello\n").content_hash(),
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }

    /// CRLF normalizes to LF before hashing, so a patch authored on
    /// Windows hashes identically to the same patch on POSIX (matches
    /// pnpm's `readNormalizedFile`).
    #[test]
    fn content_hash_normalizes_crlf_to_lf() {
        assert_eq!(
            patch_with_content("hello\r\n").content_hash(),
            patch_with_content("hello\n").content_hash(),
        );
    }

    /// pnpm's `verifyPatches` contract: a declared key that matches no
    /// installed package fails the install, and `allowUnusedPatches`
    /// downgrades exactly that failure to a warning.
    #[test]
    fn unused_patch_key_fails_the_install_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("patches")).unwrap();
        std::fs::write(dir.path().join("patches/ghost.patch"), "").unwrap();
        // `extra` lands inside the `pnpm` object, after the closing
        // brace of `patchedDependencies`.
        let manifest = |extra: &str| {
            let json = String::from(
                r#"{"pnpm":{"patchedDependencies":{"ghost@1.0.0":"patches/ghost.patch"}"#,
            ) + extra
                + "}}";
            std::fs::write(dir.path().join("package.json"), json).unwrap();
        };
        let mut graph = aube_lockfile::LockfileGraph {
            packages: [(
                "is-number@6.0.0".to_string(),
                aube_lockfile::LockedPackage {
                    name: "is-number".into(),
                    version: "6.0.0".into(),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };

        manifest("");
        let err = record_patches_on_graph(dir.path(), &mut graph)
            .expect_err("an unused patch key must fail the install");
        assert_eq!(
            err.code().map(|c| c.to_string()).as_deref(),
            Some(aube_codes::errors::ERR_AUBE_UNUSED_PATCH),
            "wrong error for an unused patch key: {err:?}"
        );

        manifest(r#","allowUnusedPatches":true"#);
        record_patches_on_graph(dir.path(), &mut graph)
            .expect("allowUnusedPatches must downgrade the failure to a warning");
    }

    #[test]
    fn upsert_writes_to_yaml_when_one_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        let written =
            upsert_patched_dependency(dir.path(), "a@1.0.0", "patches/a@1.0.0.patch").unwrap();
        assert_eq!(written, dir.path().join("pnpm-workspace.yaml"));
    }

    #[test]
    fn upsert_writes_to_package_json_when_no_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        let written =
            upsert_patched_dependency(dir.path(), "a@1.0.0", "patches/a@1.0.0.patch").unwrap();
        assert_eq!(written, dir.path().join("package.json"));
    }

    #[test]
    fn upsert_writes_to_aube_namespace_when_no_pnpm_in_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        upsert_patched_dependency(dir.path(), "a@1.0.0", "patches/a@1.0.0.patch").unwrap();
        let manifest = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(
            manifest.contains("\"aube\""),
            "expected aube namespace, got:\n{manifest}"
        );
        assert!(
            !manifest.contains("\"pnpm\""),
            "should not introduce pnpm namespace, got:\n{manifest}"
        );
        assert!(manifest.contains("\"patchedDependencies\""));
    }

    #[test]
    fn load_reads_bun_top_level_patched_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("patches")).unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "patchedDependencies": {
    "is-number@7.0.0": "patches/is-number.patch"
  }
}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("patches/is-number.patch"),
            "diff --git a/index.js b/index.js\n",
        )
        .unwrap();

        let patches = load_patches(dir.path()).unwrap();
        assert_eq!(
            patches
                .get("is-number@7.0.0")
                .map(|p| p.path.strip_prefix(dir.path()).unwrap()),
            Some(Path::new("patches/is-number.patch"))
        );
    }

    #[test]
    fn upsert_collapses_shadow_when_other_namespace_holds_stale_entry() {
        // A pnpm-aware tool can add a `pnpm` namespace after aube has
        // already populated `aube.patchedDependencies`. Without the
        // merge-and-collapse below, the next `aube patch-commit` would
        // write to `pnpm.patchedDependencies` while the stale
        // `aube.patchedDependencies.<key>` entry shadowed it on read
        // (aube.* wins on conflict). Pin the post-write invariant:
        // exactly one namespace holds the entry.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"aube\":{\"patchedDependencies\":{\"a@1.0.0\":\"patches/old.patch\"}},\"pnpm\":{\"someKey\":1}}\n",
        )
        .unwrap();
        upsert_patched_dependency(dir.path(), "a@1.0.0", "patches/new.patch").unwrap();
        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Entry migrated to pnpm.patchedDependencies, with the new value.
        assert_eq!(
            parsed["pnpm"]["patchedDependencies"]["a@1.0.0"],
            "patches/new.patch"
        );
        // Stale aube.patchedDependencies entry is gone — no shadow.
        assert!(parsed["aube"]["patchedDependencies"].is_null());
        // The user's other pnpm config is preserved.
        assert_eq!(parsed["pnpm"]["someKey"], 1);
    }

    #[test]
    fn upsert_writes_to_pnpm_namespace_when_pnpm_already_present() {
        let dir = tempfile::tempdir().unwrap();
        // User already has a pnpm-namespaced setting (allowBuilds);
        // a new patch should land in the same pnpm bucket so we don't
        // fragment their config across two namespaces.
        std::fs::write(
            dir.path().join("package.json"),
            "{\"pnpm\":{\"allowBuilds\":{\"esbuild\":true}}}\n",
        )
        .unwrap();
        upsert_patched_dependency(dir.path(), "a@1.0.0", "patches/a@1.0.0.patch").unwrap();
        let manifest = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(manifest.contains("\"pnpm\""));
        assert!(
            !manifest.contains("\"aube\""),
            "should not introduce aube namespace alongside pnpm: {manifest}"
        );
        // The patch entry must be inside pnpm.
        let parsed: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(parsed["pnpm"]["patchedDependencies"]["a@1.0.0"].is_string());
    }

    #[test]
    fn remove_deletes_bun_top_level_patched_dependency() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "patchedDependencies": {
    "a@1.0.0": "patches/a.patch",
    "b@2.0.0": "patches/b.patch"
  }
}
"#,
        )
        .unwrap();

        let rewritten = remove_patched_dependency(dir.path(), "a@1.0.0").unwrap();
        assert_eq!(rewritten, vec![dir.path().join("package.json")]);
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
        )
        .unwrap();
        assert!(parsed["patchedDependencies"]["a@1.0.0"].is_null());
        assert_eq!(parsed["patchedDependencies"]["b@2.0.0"], "patches/b.patch");
    }

    #[test]
    fn remove_drops_empty_bun_top_level_patched_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "patchedDependencies": {
    "a@1.0.0": "patches/a.patch"
  }
}
"#,
        )
        .unwrap();

        remove_patched_dependency(dir.path(), "a@1.0.0").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
        )
        .unwrap();
        assert!(parsed["patchedDependencies"].is_null());
    }

    #[test]
    fn remove_leaves_empty_bun_top_level_map_when_key_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"patchedDependencies\": {}\n}\n",
        )
        .unwrap();

        let rewritten = remove_patched_dependency(dir.path(), "missing@9.9.9").unwrap();
        assert!(rewritten.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
            "{\n  \"patchedDependencies\": {}\n}\n"
        );
    }

    #[test]
    fn remove_reads_bom_prefixed_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "\u{feff}{\n  \"patchedDependencies\": {\n    \"a@1.0.0\": \"patches/a.patch\"\n  }\n}\n",
        )
        .unwrap();

        let rewritten = remove_patched_dependency(dir.path(), "a@1.0.0").unwrap();
        assert_eq!(rewritten, vec![dir.path().join("package.json")]);
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
        )
        .unwrap();
        assert!(parsed["patchedDependencies"].is_null());
    }

    #[test]
    fn load_reads_lockfile_patched_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".yarn/patches")).unwrap();
        std::fs::write(
            dir.path().join(".yarn/patches/is-number.patch"),
            "diff --git a/index.js b/index.js\n",
        )
        .unwrap();

        // A lockfile-carried patch entry is merged into the declared set
        // (keyed by the verbatim source key). Resolving it against a graph
        // is covered by `aube_lockfile::patch_groups` unit tests.
        let resolved = load_patches_with_lockfile_entries(
            dir.path(),
            &BTreeMap::from([(
                "is-number@7.0.0".to_string(),
                ".yarn/patches/is-number.patch".to_string(),
            )]),
        )
        .unwrap();

        let entry = resolved.get("is-number@7.0.0").expect("entry present");
        assert_eq!(entry.content, "diff --git a/index.js b/index.js\n");
        assert_eq!(entry.rel, ".yarn/patches/is-number.patch");
    }

    #[test]
    fn remove_returns_each_rewritten_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"pnpm\":{\"patchedDependencies\":{\"a@1.0.0\":\"patches/a@1.0.0.patch\"}}}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        let rewritten = remove_patched_dependency(dir.path(), "a@1.0.0").unwrap();
        let names: Vec<String> = rewritten
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, vec!["pnpm-workspace.yaml", "package.json"]);
    }

    #[test]
    fn remove_returns_empty_when_neither_file_holds_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        let rewritten = remove_patched_dependency(dir.path(), "missing@9.9.9").unwrap();
        assert!(rewritten.is_empty());
    }
}
