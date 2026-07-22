use crate::{Error, ResolveTask};
use aube_lockfile::{LocalSource, LockedPackage};
use aube_registry::client::RegistryClient;
use aube_util::path::normalize_lexical;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Rewrite a `LocalSource` whose path is relative to `importer_root`
/// into one whose path is relative to `project_root`, so downstream
/// code (install.rs, linker) can resolve the target with a single
/// `project_root.join(rel)` regardless of which workspace importer
/// declared it.
///
/// Both the join-then-diff intermediate and the returned path are
/// lexically normalized — `Path::join` and `pathdiff::diff_paths`
/// leave `..` components in place, which means `packages/app` +
/// `../../vendor-dir` would otherwise produce
/// `packages/app/../../vendor-dir`. That non-canonical form fed into
/// `dep_path`'s hash would produce a different key for every
/// importer declaring the same target, and would also leak into the
/// lockfile's `version:` string.
pub(crate) fn rebase_local(
    local: &LocalSource,
    importer_root: &Path,
    project_root: &Path,
) -> LocalSource {
    // The fast path: importer_root == project_root. Root-importer
    // installs take this branch, which is also the single-project
    // case — no rewrite needed and we preserve the raw specifier
    // bytes for a byte-identical lockfile round-trip.
    if importer_root == project_root {
        if let LocalSource::Exec(path) = local {
            return LocalSource::Exec(normalize_lexical(path));
        }
        return local.clone();
    }
    let Some(local_path) = local.path() else {
        // Non-path sources (git) have nothing to rebase.
        return local.clone();
    };
    let abs = normalize_lexical(&importer_root.join(local_path));
    let rebased = pathdiff::diff_paths(&abs, project_root).map_or(abs, |p| normalize_lexical(&p));
    match local {
        LocalSource::Directory(_) => LocalSource::Directory(rebased),
        LocalSource::Tarball(_) => LocalSource::Tarball(rebased),
        LocalSource::Link(_) => LocalSource::Link(rebased),
        LocalSource::Portal(_) => LocalSource::Portal(rebased),
        LocalSource::Exec(_) => LocalSource::Exec(rebased),
        LocalSource::Git(_) | LocalSource::RemoteTarball(_) => local.clone(),
    }
}

/// Resolve an `exec:` generator path and reject scripts outside the project root.
pub fn resolve_exec_script_path(
    local: &LocalSource,
    project_root: &Path,
) -> Result<PathBuf, String> {
    let LocalSource::Exec(rel) = local else {
        return Err("resolve_exec_script_path called on non-exec source".to_string());
    };
    let script = project_root.join(rel);
    if !script.is_file() {
        return Err(format!("{} is not a file", script.display()));
    }
    let canonical_root = project_root
        .canonicalize()
        .map_err(|e| format!("canonicalize project root {}: {e}", project_root.display()))?;
    let canonical_script = script
        .canonicalize()
        .map_err(|e| format!("canonicalize exec script {}: {e}", script.display()))?;
    if !canonical_script.starts_with(&canonical_root) {
        return Err(format!(
            "{} resolves outside project root {}",
            script.display(),
            canonical_root.display()
        ));
    }
    Ok(canonical_script)
}

/// Walk a gzipped npm tarball once and return the raw bytes of its
/// top-level `package.json` entry. The wrapper directory name varies
/// (`package/`, but also e.g. GitHub's `owner-repo-<sha>/`), so we
/// match on the entry's basename plus a 2-component depth check
/// rather than a hardcoded prefix. Errors come back as plain
/// `String`s so each caller can wrap them with its own package
/// identity in whatever error type it prefers — used by both the
/// `file:` tarball path (`read_local_manifest`) and the remote
/// tarball resolver (`resolve_remote_tarball`).
/// Hard upper bound on the bytes read from the gzipped tarball stream
/// while looking for `package.json`. A 64 MiB ceiling is far above any
/// real npm package and keeps a hostile gzip bomb from amplifying into
/// arbitrary RAM. Mirrors `aube-store::MAX_TARBALL_DECOMPRESSED_BYTES`
/// in spirit — the resolver path was missed in the original cap pass.
const MAX_RESOLVE_TARBALL_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESOLVE_PACKAGE_JSON_BYTES: u64 = 8 * 1024 * 1024;

fn read_tarball_package_json(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    // Cap on the DECOMPRESSED output of the gzip stream so a hostile
    // tarball with large dummy entries before `package.json` cannot
    // amplify the fixed compressed input window into arbitrary RAM.
    // `bytes.take` would only bound the compressed read, which the
    // decoder is free to expand without ceiling.
    let gz = flate2::read::GzDecoder::new(bytes);
    let capped = gz.take(MAX_RESOLVE_TARBALL_DECOMPRESSED_BYTES);
    let mut archive = tar::Archive::new(capped);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let entry_path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
        if entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "package.json")
            && entry_path.components().count() == 2
        {
            let mut buf = Vec::new();
            entry
                .take(MAX_RESOLVE_PACKAGE_JSON_BYTES + 1)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            if buf.len() as u64 > MAX_RESOLVE_PACKAGE_JSON_BYTES {
                return Err("package.json exceeds 8 MiB cap".to_string());
            }
            return Ok(buf);
        }
    }
    Err("tarball has no top-level package.json".to_string())
}

/// Read the `package.json` of a `file:` / `link:` target to discover
/// the real package name, version, and production dependencies.
///
/// For `LocalSource::Directory`, `LocalSource::Link`, and
/// `LocalSource::Portal` we read the target dir's `package.json`
/// directly. For `LocalSource::Tarball` we open the `.tgz`, find the
/// first `*/package.json` entry, and parse its contents without
/// extracting the rest of the archive.
pub(crate) fn read_local_manifest(
    local: &LocalSource,
    importer_root: &Path,
) -> Result<(String, String, BTreeMap<String, String>), Error> {
    let Some(local_path) = local.path() else {
        return Err(Error::Registry(
            local.specifier(),
            "read_local_manifest called on non-path source".to_string(),
        ));
    };
    let path = importer_root.join(local_path);

    let content = match local {
        LocalSource::Directory(_) | LocalSource::Link(_) | LocalSource::Portal(_) => {
            std::fs::read(path.join("package.json"))
                .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?
        }
        LocalSource::Tarball(_) => {
            let bytes = std::fs::read(&path)
                .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?;
            read_tarball_package_json(&bytes).map_err(|e| Error::Registry(local.specifier(), e))?
        }
        LocalSource::Exec(_) | LocalSource::Git(_) | LocalSource::RemoteTarball(_) => {
            return Err(Error::Registry(
                local.specifier(),
                "read_local_manifest: generated or remote source handled separately".to_string(),
            ));
        }
    };

    let pj: aube_manifest::PackageJson = sonic_rs::from_slice(&content)
        .or_else(|_| serde_json::from_slice(&content))
        .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?;
    Ok((
        pj.name.unwrap_or_default(),
        pj.version.unwrap_or_else(|| "0.0.0".to_string()),
        pj.dependencies,
    ))
}

pub(crate) async fn resolve_exec_manifest(
    name: &str,
    local: &LocalSource,
    project_root: &Path,
) -> Result<(String, BTreeMap<String, String>), Error> {
    let LocalSource::Exec(_) = local else {
        return Err(Error::Registry(
            name.to_string(),
            "resolve_exec_manifest called on non-exec source".to_string(),
        ));
    };
    let script = resolve_exec_script_path(local, project_root).map_err(|e| {
        Error::Registry(
            name.to_string(),
            format!("exec dependency {}: {e}", local.specifier()),
        )
    })?;

    let temp = tempfile::Builder::new()
        .prefix("aube-exec-resolve-")
        .tempdir()
        .map_err(|e| Error::Registry(name.to_string(), e.to_string()))?;
    let build_dir = temp.path().join("build");
    let temp_dir = temp.path().join("temp");
    std::fs::create_dir_all(&build_dir)
        .map_err(|e| Error::Registry(name.to_string(), e.to_string()))?;
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| Error::Registry(name.to_string(), e.to_string()))?;

    let env = serde_json::json!({
        "tempDir": temp_dir,
        "buildDir": build_dir,
        "locator": format!("{name}@{}", local.specifier()),
    });
    let status = tokio::process::Command::new("node")
        .arg("-e")
        .arg(crate::YARN_EXEC_WRAPPER)
        .arg(&script)
        .env("AUBE_YARN_EXEC_ENV", env.to_string())
        .current_dir(project_root)
        .status()
        .await
        .map_err(|e| {
            Error::Registry(
                name.to_string(),
                format!("execute {} with Node.js from PATH: {e}", local.specifier()),
            )
        })?;
    if !status.success() {
        return Err(Error::Registry(
            name.to_string(),
            format!(
                "exec dependency {} failed with status {status}",
                local.specifier()
            ),
        ));
    }

    let content = std::fs::read(build_dir.join("package.json")).map_err(|e| {
        Error::Registry(
            name.to_string(),
            format!("read generated package.json for {}: {e}", local.specifier()),
        )
    })?;
    let pj: aube_manifest::PackageJson = sonic_rs::from_slice(&content)
        .or_else(|_| serde_json::from_slice(&content))
        .map_err(|e| Error::Registry(name.to_string(), e.to_string()))?;
    Ok((
        pj.version.unwrap_or_else(|| "0.0.0".to_string()),
        pj.dependencies,
    ))
}

pub(crate) fn dep_path_for(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// Match specifier prefixes that resolve to a non-registry source
/// (`file:`, `link:`, `portal:`, `exec:`, or a git URL form). Used
/// by the resolver to decide whether to dispatch the local/git branch
/// instead of the normal version-range lookup.
pub(crate) fn is_non_registry_specifier(s: &str) -> bool {
    if s.starts_with("link:") {
        return true;
    }
    if s.starts_with("portal:") {
        return true;
    }
    if s.starts_with("exec:") {
        return true;
    }
    // Git first so `https://host/repo.git` dispatches the git branch
    // rather than the broader bare-http tarball branch below.
    if aube_lockfile::parse_git_spec(s).is_some() {
        return true;
    }
    // Any remaining bare `http(s)://` URL is a tarball URL, per npm
    // semantics — the `.tgz` suffix is not required.
    if aube_lockfile::LocalSource::looks_like_remote_tarball_url(s) {
        return true;
    }
    // `file:` is a local-path prefix only when it *isn't* also a git
    // URL form — parse_git_spec already matched `file://…/repo.git`
    // above, so anything that reaches here is treated as a path.
    s.starts_with("file:")
}

pub(crate) fn should_block_exotic_subdep(
    task: &ResolveTask,
    resolved: &BTreeMap<String, LockedPackage>,
    block_exotic_subdeps: bool,
) -> bool {
    block_exotic_subdeps
        && !task.is_root
        && !task
            .parent
            .as_ref()
            .and_then(|parent| resolved.get(parent))
            .is_some_and(|pkg| {
                matches!(
                    pkg.local_source,
                    Some(LocalSource::Directory(_))
                        | Some(LocalSource::Link(_))
                        | Some(LocalSource::Portal(_))
                        | Some(LocalSource::Exec(_))
                )
            })
}

/// Pick the lockfile source representation for a *resolved* hosted-git
/// dependency. pnpm records a github / gitlab / bitbucket dep pinned to
/// a 40-char commit SHA as a **codeload tarball** (`RemoteTarball`) —
/// not a `git` resolution — whenever a flat HTTPS archive URL exists
/// (`codeload_url`) and there's no `&path:` subdir selector. aube
/// already *fetches* that tarball; emitting it as `RemoteTarball` makes
/// the written lockfile match pnpm (codeload key + `version:` +
/// `resolution: {tarball, gitHosted}`) instead of the divergent
/// `<url>.git#<sha>` / `resolution: {type: git, repo, commit}` form.
///
/// Falls back to `Git` for: non-hosted or `git+ssh://` sources (no
/// codeload URL — pnpm keeps those as `type: git` too), branch/tag refs
/// that never pinned to a SHA, and `&path:` subpath selectors (a flat
/// tarball can't address a repo subdirectory).
fn hosted_git_local_source(
    original_url: String,
    committish: Option<String>,
    resolved: String,
    subpath: Option<String>,
    integrity: Option<String>,
    codeload_url: Option<&str>,
) -> LocalSource {
    match (subpath.as_deref(), codeload_url) {
        (None, Some(codeload)) => LocalSource::RemoteTarball(aube_lockfile::RemoteTarballSource {
            url: codeload.to_string(),
            integrity: integrity.unwrap_or_default(),
            git_hosted: true,
        }),
        _ => LocalSource::Git(aube_lockfile::GitSource {
            url: original_url,
            committish,
            resolved,
            integrity,
            subpath,
        }),
    }
}

fn read_git_package_manifest(
    name: &str,
    pkg_root: &Path,
    location: &str,
    subpath: Option<&str>,
) -> Result<(String, BTreeMap<String, String>), Error> {
    let where_ = subpath.map(|s| format!(" at /{s}")).unwrap_or_default();
    let meta = std::fs::metadata(pkg_root).map_err(|e| {
        Error::Registry(
            name.to_string(),
            format!("stat git package root in {location}{where_}: {e}"),
        )
    })?;
    if !meta.is_dir() {
        return Err(Error::Registry(
            name.to_string(),
            format!(
                "git package root in {location}{where_} is not a directory: {}",
                pkg_root.display()
            ),
        ));
    }

    let manifest_path = pkg_root.join("package.json");
    let manifest_bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                package = name,
                root = %pkg_root.display(),
                "git dependency has no package.json; resolving as version 0.0.0 with no dependencies",
            );
            return Ok(("0.0.0".to_string(), BTreeMap::new()));
        }
        Err(e) => {
            return Err(Error::Registry(
                name.to_string(),
                format!("read package.json in {location}{where_}: {e}"),
            ));
        }
    };
    let pj: aube_manifest::PackageJson = sonic_rs::from_slice(&manifest_bytes)
        .or_else(|_| serde_json::from_slice(&manifest_bytes))
        .map_err(|e| {
            Error::Registry(
                name.to_string(),
                format!("parse package.json in {location}{where_}: {e}"),
            )
        })?;
    Ok((
        pj.version.unwrap_or_else(|| "0.0.0".to_string()),
        pj.dependencies,
    ))
}

/// Turn a raw `GitSource` (committish parsed from the user's
/// specifier, empty `resolved`) into a fully-resolved one by either
/// fetching a hosted-tarball over HTTPS (github / gitlab / bitbucket
/// public reads, matching what npm `pacote` and pnpm
/// `gitHostedTarballFetcher` do) or, for any other host or any
/// codeload-unreachable case, falling back to `git ls-remote` +
/// shallow clone. The materialized tree lives in a commit-keyed temp
/// directory shared with install-time materialization, so the same
/// extraction or clone is never repeated within a single `aube
/// install`.
///
/// Hosted-tarball routing matches npm/pnpm semantics: the lockfile's
/// stored `url` is canonical-identity only — even when it carries an
/// SSH form the user has no key for, we re-derive an HTTPS URL from
/// the `(host, owner, repo)` tuple at fetch time. Returns the
/// original URL unchanged in `LocalSource::Git.url` so a subsequent
/// `aube install` produces the same lockfile bytes (cross-tool
/// compat with pnpm / npm / yarn).
pub(crate) async fn resolve_git_source(
    name: &str,
    git: &aube_lockfile::GitSource,
    shallow: bool,
    client: Option<&RegistryClient>,
) -> Result<
    (
        LocalSource,
        String,
        BTreeMap<String, String>,
        Option<String>,
    ),
    Error,
> {
    let original_url = git.url.clone();
    let committish = git.committish.clone();
    let subpath = git.subpath.clone();
    let hosted = aube_lockfile::parse_hosted_git(&original_url);
    // Use the HTTPS form when talking to git for hosted hosts — the
    // lockfile-canonical `git+ssh://git@…` URL would dial SSH and
    // fail for users with no `~/.ssh/`. Non-hosted URLs go through
    // unchanged so SSH-only setups keep working.
    let runtime_url = hosted
        .as_ref()
        .map(|h| h.https_url())
        .unwrap_or_else(|| original_url.clone());

    // Resolve the committish to a 40-char SHA. `git_resolve_ref`
    // short-circuits on a SHA and shells `git ls-remote` for branch /
    // tag / HEAD. Passing the rewritten HTTPS URL means hosted
    // branch/tag refs are pinnable from a host with no SSH key
    // configured.
    let runtime_url_for_ref = runtime_url.clone();
    let committish_for_ref = committish.clone();
    let name_for_ref = name.to_string();
    let resolved_sha = tokio::task::spawn_blocking(move || -> Result<String, Error> {
        let seed = aube_store::git_resolve_ref(&runtime_url_for_ref, committish_for_ref.as_deref())
            .map_err(|e| Error::Registry(name_for_ref.clone(), e.to_string()))?;
        // Only full SHAs survive — abbreviated user-written prefixes
        // come back unchanged from `git_resolve_ref` and need to fall
        // through to the clone path so `git checkout <prefix>` can
        // expand them.
        Ok(seed)
    })
    .await
    .map_err(|e| {
        Error::Registry(
            name.to_string(),
            format!("git ls-remote task panicked: {e}"),
        )
    })??;

    let codeload_url = hosted.as_ref().and_then(|h| h.tarball_url(&resolved_sha));

    // Cache hit fast path: skip the HTTPS round-trip when a prior call
    // (the resolver's earlier visit to this dep, or a previous install)
    // already populated the codeload cache. Mirrors `git_shallow_clone`'s
    // top-of-function reuse check.
    if codeload_url.is_some()
        && git.integrity.is_some()
        && let Some((clone_dir, _head_sha)) = aube_store::codeload_cache_lookup(
            &original_url,
            &resolved_sha,
            git.integrity.as_deref(),
        )
    {
        let integrity = aube_store::codeload_cache_integrity(
            &original_url,
            &resolved_sha,
            git.integrity.as_deref(),
        );
        let pkg_root = match &subpath {
            Some(sub) => clone_dir.join(sub),
            None => clone_dir.clone(),
        };
        let (version, deps) = read_git_package_manifest(
            name,
            &pkg_root,
            "cached codeload extract",
            subpath.as_deref(),
        )?;
        return Ok((
            hosted_git_local_source(
                original_url,
                committish,
                resolved_sha,
                subpath,
                git.integrity.clone(),
                codeload_url.as_deref(),
            ),
            version,
            deps,
            integrity,
        ));
    }

    // Try the codeload fast path when applicable. `client` is None for
    // resolve paths that don't have a registry client wired up
    // (`aube import`'s lockfile-only flow); those just fall through.
    if let (Some(c), Some(url_to_fetch)) = (client, codeload_url.as_deref()) {
        match c.fetch_tarball_bytes(url_to_fetch).await {
            Ok(bytes) => {
                // Extract into the commit-keyed cache and read the
                // (possibly subpath-scoped) `package.json` like the
                // clone path does. Return the original lockfile URL
                // in `LocalSource::Git.url` for cross-tool round-trip.
                let bytes_vec = bytes.to_vec();
                if let Some(pinned) = &git.integrity {
                    aube_store::verify_integrity(&bytes_vec, pinned)
                        .map_err(|e| Error::Registry(name.to_string(), e.to_string()))?;
                }
                let integrity = git
                    .integrity
                    .clone()
                    .unwrap_or_else(|| aube_store::sha512_integrity(&bytes_vec));
                let url_for_extract = original_url.clone();
                let sha_for_extract = resolved_sha.clone();
                let integrity_for_extract = integrity.clone();
                let subpath_for_extract = subpath.clone();
                let name_for_extract = name.to_string();
                let extracted = tokio::task::spawn_blocking(move || -> Result<_, Error> {
                    let (clone_dir, resolved) = aube_store::extract_codeload_tarball(
                        &bytes_vec,
                        &url_for_extract,
                        &sha_for_extract,
                        Some(&integrity_for_extract),
                    )
                    .map_err(|e| Error::Registry(name_for_extract.clone(), e.to_string()))?;
                    let pkg_root = match &subpath_for_extract {
                        Some(sub) => clone_dir.join(sub),
                        None => clone_dir.clone(),
                    };
                    let (version, deps) = read_git_package_manifest(
                        &name_for_extract,
                        &pkg_root,
                        "codeload extract",
                        subpath_for_extract.as_deref(),
                    )?;
                    Ok((resolved, version, deps))
                })
                .await
                .map_err(|e| {
                    Error::Registry(name.to_string(), format!("codeload extract panicked: {e}"))
                })?;
                let integrity = aube_store::sha512_integrity(&bytes);
                match extracted {
                    Ok((resolved, version, deps)) => {
                        return Ok((
                            hosted_git_local_source(
                                original_url,
                                committish,
                                resolved,
                                subpath,
                                Some(integrity.clone()),
                                Some(url_to_fetch),
                            ),
                            version,
                            deps,
                            Some(integrity),
                        ));
                    }
                    Err(e) => {
                        // Mirror the installer: a corrupt or
                        // unexpectedly-shaped tarball (CDN hiccup,
                        // unsafe-path rejection, Windows symlink) falls
                        // through to `git clone`, which inherits the
                        // user's git credential helper and can write
                        // symlinks via git's admin-aware path.
                        tracing::debug!(
                            name,
                            "codeload extract failed, falling back to git clone: {e}",
                        );
                    }
                }
            }
            Err(e) => {
                // Codeload 404s on private repos (it doesn't accept
                // npm-registry auth) — fall through to `git
                // clone`, which inherits the user's git credential
                // helper / ssh keys for private access.
                tracing::debug!(
                    name,
                    url = %aube_util::url::redact_url(url_to_fetch),
                    "codeload fetch failed, falling back to git clone: {e}",
                );
            }
        }
    }

    // Fallback: shallow git clone over the rewritten HTTPS URL (or the
    // original URL for non-hosted hosts). Same `spawn_blocking` dance
    // the original implementation used.
    let runtime_url_for_clone = runtime_url;
    let original_url_for_lockfile = original_url.clone();
    let resolved_sha_for_clone = resolved_sha.clone();
    let subpath_for_clone = subpath.clone();
    let name_for_clone = name.to_string();
    let (local, version, deps) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
        let (clone_dir, resolved) =
            aube_store::git_shallow_clone(&runtime_url_for_clone, &resolved_sha_for_clone, shallow)
                .map_err(|e| Error::Registry(name_for_clone.clone(), e.to_string()))?;
        let pkg_root = match &subpath_for_clone {
            Some(sub) => clone_dir.join(sub),
            None => clone_dir.clone(),
        };
        let (version, deps) = read_git_package_manifest(
            &name_for_clone,
            &pkg_root,
            "clone",
            subpath_for_clone.as_deref(),
        )?;
        Ok((
            LocalSource::Git(aube_lockfile::GitSource {
                url: original_url_for_lockfile,
                committish,
                resolved,
                integrity: None,
                subpath: subpath_for_clone,
            }),
            version,
            deps,
        ))
    })
    .await
    .map_err(|e| Error::Registry(name.to_string(), format!("git task panicked: {e}")))??;
    Ok((local, version, deps, None))
}

/// Fetch a remote tarball URL, compute its sha512 integrity, and read
/// the enclosed `package.json` for version + transitive deps. Returns
/// a fully-populated `LocalSource::RemoteTarball` alongside the
/// manifest tuple the resolver's local-dep branch expects.
pub(crate) async fn resolve_remote_tarball(
    name: &str,
    tarball: &aube_lockfile::RemoteTarballSource,
    client: &RegistryClient,
) -> Result<(LocalSource, String, BTreeMap<String, String>), Error> {
    let bytes = client
        .fetch_tarball_bytes(&tarball.url)
        .await
        .map_err(|e| {
            Error::Registry(
                name.to_string(),
                format!("fetch {}: {e}", aube_util::url::redact_url(&tarball.url)),
            )
        })?;
    let name_owned = name.to_string();
    let url = aube_util::url::redact_url(&tarball.url);
    let (integrity, version, deps) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
        let integrity = aube_store::sha512_integrity(&bytes);

        // Walk the tarball once to pull out the top-level
        // `package.json` (wrapper name varies, so the helper looks
        // at the first path component's basename, not a hardcoded
        // `package/package.json`).
        let manifest_bytes = read_tarball_package_json(&bytes)
            .map_err(|e| Error::Registry(name_owned.clone(), format!("tarball {url}: {e}")))?;
        let pj: aube_manifest::PackageJson = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        let version = pj.version.unwrap_or_else(|| "0.0.0".to_string());
        Ok((integrity, version, pj.dependencies))
    })
    .await
    .map_err(|e| Error::Registry(name.to_string(), format!("tarball task panicked: {e}")))??;
    Ok((
        LocalSource::RemoteTarball(aube_lockfile::RemoteTarballSource {
            url: tarball.url.clone(),
            integrity,
            git_hosted: tarball.git_hosted,
        }),
        version,
        deps,
    ))
}

#[cfg(test)]
mod rebase_local_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn workspace_file_climbs_out_of_importer_to_root_sibling() {
        // packages/app importer declares `file:../../vendor-dir`.
        // Expected result: `vendor-dir` (workspace-root relative),
        // collapsed down from the intermediate
        // `packages/app/../../vendor-dir` form.
        let local = LocalSource::Directory(PathBuf::from("../../vendor-dir"));
        let rebased = rebase_local(&local, Path::new("packages/app"), Path::new(""));
        match rebased {
            LocalSource::Directory(p) => assert_eq!(p, PathBuf::from("vendor-dir")),
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn two_importers_referencing_same_target_collide_on_dep_path() {
        // Both importers end up pointing at the same on-disk path —
        // the encoded dep_path must match so they de-dupe in the
        // lockfile.
        let a = rebase_local(
            &LocalSource::Directory(PathBuf::from("../../vendor-dir")),
            Path::new("packages/app"),
            Path::new(""),
        );
        let b = rebase_local(
            &LocalSource::Directory(PathBuf::from("../vendor-dir")),
            Path::new("packages"),
            Path::new(""),
        );
        assert_eq!(a.dep_path("vendor-dir"), b.dep_path("vendor-dir"));
    }

    #[test]
    fn root_and_transitive_exec_paths_collide_on_dep_path() {
        let root = rebase_local(
            &LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js")),
            Path::new(""),
            Path::new(""),
        );
        let transitive = rebase_local(
            &LocalSource::Exec(PathBuf::from("../../scripts/generate-exec.js")),
            Path::new("packages/portal"),
            Path::new(""),
        );
        assert_eq!(root.dep_path("exec-pkg"), transitive.dep_path("exec-pkg"));
    }

    #[test]
    fn normalize_preserves_unresolvable_leading_parent() {
        // `..` at the root of the project is still meaningful —
        // don't silently drop it.
        assert_eq!(
            normalize_lexical(Path::new("../vendor")),
            PathBuf::from("../vendor")
        );
    }

    #[test]
    fn dep_path_and_specifier_use_posix_separators() {
        // Backslash-separated input (as Windows would store) must
        // hash and render the same as a forward-slash equivalent so
        // a checked-in lockfile resolves identically on either OS.
        let win = LocalSource::Directory(PathBuf::from("vendor\\nested\\dir"));
        let unix = LocalSource::Directory(PathBuf::from("vendor/nested/dir"));
        assert_eq!(win.dep_path("foo"), unix.dep_path("foo"));
        assert_eq!(win.specifier(), "file:vendor/nested/dir");
        assert_eq!(unix.specifier(), "file:vendor/nested/dir");
    }

    #[test]
    fn exec_script_must_stay_inside_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        let outside = temp.path().join("outside.js");
        std::fs::create_dir(&project_root).unwrap();
        std::fs::write(&outside, "").unwrap();

        let local = LocalSource::Exec(PathBuf::from("../outside.js"));
        let err = resolve_exec_script_path(&local, &project_root).unwrap_err();
        assert!(err.contains("resolves outside project root"), "{err}");
    }

    #[test]
    fn exec_script_inside_project_root_is_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        let script_dir = project_root.join("scripts");
        let script = script_dir.join("generate.js");
        std::fs::create_dir_all(&script_dir).unwrap();
        std::fs::write(&script, "").unwrap();

        let local = LocalSource::Exec(PathBuf::from("scripts/generate.js"));
        let resolved = resolve_exec_script_path(&local, &project_root).unwrap();
        assert_eq!(resolved, script.canonicalize().unwrap());
    }
}

#[cfg(test)]
mod cve_audit_tarball_bomb {
    use super::*;
    use std::io::Write;

    fn build_zero_tarball(uncompressed_size: usize) -> Vec<u8> {
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let payload = vec![0u8; uncompressed_size];
            let mut header = tar::Header::new_gnu();
            header.set_path("pkg/package.json").unwrap();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &payload[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::best());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }
        gz
    }

    fn build_dummy_then_package_json(dummy_size: usize) -> Vec<u8> {
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let dummy = vec![0u8; dummy_size];
            let mut h1 = tar::Header::new_gnu();
            h1.set_path("pkg/dummy.bin").unwrap();
            h1.set_size(dummy.len() as u64);
            h1.set_mode(0o644);
            h1.set_cksum();
            builder.append(&h1, &dummy[..]).unwrap();
            let manifest = b"{\"name\":\"x\",\"version\":\"0.0.1\"}";
            let mut h2 = tar::Header::new_gnu();
            h2.set_path("pkg/package.json").unwrap();
            h2.set_size(manifest.len() as u64);
            h2.set_mode(0o644);
            h2.set_cksum();
            builder.append(&h2, &manifest[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::best());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }
        gz
    }

    #[test]
    fn read_tarball_package_json_rejects_decompression_bomb() {
        let bomb = build_zero_tarball(200 * 1024 * 1024);
        assert!(
            bomb.len() < 400 * 1024,
            "compressed bomb too large to call this an amplification: {}",
            bomb.len()
        );
        let result = read_tarball_package_json(&bomb);
        assert!(
            result.is_err(),
            "200 MiB decompressed payload must be rejected by the cap, got {:?}",
            result.as_ref().map(|b| b.len())
        );
    }

    #[test]
    fn read_tarball_package_json_rejects_dummy_entry_amplification() {
        let bomb = build_dummy_then_package_json(200 * 1024 * 1024);
        assert!(
            bomb.len() < 400 * 1024,
            "compressed multi-entry bomb too large: {}",
            bomb.len()
        );
        let result = read_tarball_package_json(&bomb);
        assert!(
            result.is_err(),
            "decompressed dummy entry preceding package.json must hit the output cap"
        );
    }
}

#[cfg(test)]
mod hosted_git_local_source_tests {
    use super::*;

    const SHA: &str = "78e559baa908942097330f7967dfbf623ebc2529";

    #[test]
    fn hosted_sha_without_subpath_becomes_codeload_remote_tarball() {
        let codeload = format!("https://codeload.github.com/xmppo/node-expat/tar.gz/{SHA}");
        let src = hosted_git_local_source(
            "git+ssh://git@github.com/xmppo/node-expat.git".to_string(),
            Some(format!("v2.4.3#{SHA}")),
            SHA.to_string(),
            None,
            Some("sha512-deadbeef".to_string()),
            Some(codeload.as_str()),
        );
        match src {
            LocalSource::RemoteTarball(t) => {
                // pnpm keys the lockfile entry by this flat tarball URL.
                assert_eq!(t.url, codeload);
                assert_eq!(t.integrity, "sha512-deadbeef");
                assert!(t.git_hosted, "codeload archives must flag gitHosted");
                // The specifier the writer threads into snapshot deps and
                // the packages key is exactly the codeload URL.
                assert_eq!(
                    LocalSource::RemoteTarball(t).specifier(),
                    codeload,
                    "specifier must be the bare codeload URL pnpm records"
                );
            }
            other => panic!("expected RemoteTarball, got {other:?}"),
        }
    }

    #[test]
    fn subpath_selector_stays_git() {
        // A flat tarball can't address a repo subdirectory, so pnpm keeps
        // `&path:` deps as `type: git`. We must too.
        let codeload = format!("https://codeload.github.com/acme/mono/tar.gz/{SHA}");
        let src = hosted_git_local_source(
            "git+ssh://git@github.com/acme/mono.git".to_string(),
            Some(SHA.to_string()),
            SHA.to_string(),
            Some("packages/leaf".to_string()),
            Some("sha512-x".to_string()),
            Some(codeload.as_str()),
        );
        match src {
            LocalSource::Git(g) => {
                assert_eq!(g.resolved, SHA);
                assert_eq!(g.subpath.as_deref(), Some("packages/leaf"));
            }
            other => panic!("expected Git with subpath, got {other:?}"),
        }
    }

    #[test]
    fn no_codeload_url_stays_git() {
        // Non-hosted / ssh-only sources have no flat archive URL; pnpm
        // records those as `type: git` and so do we.
        let src = hosted_git_local_source(
            "git+ssh://git@example.com/internal/dep.git".to_string(),
            Some(SHA.to_string()),
            SHA.to_string(),
            None,
            Some("sha512-y".to_string()),
            None,
        );
        match src {
            LocalSource::Git(g) => {
                assert_eq!(g.url, "git+ssh://git@example.com/internal/dep.git");
                assert_eq!(g.integrity.as_deref(), Some("sha512-y"));
            }
            other => panic!("expected Git, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod git_package_manifest_tests {
    use super::*;

    #[test]
    fn missing_git_package_json_defaults_to_empty_manifest() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("schema.json"), "{}").unwrap();

        let (version, deps) =
            read_git_package_manifest("asset-only", temp.path(), "clone", None).unwrap();

        assert_eq!(version, "0.0.0");
        assert!(deps.is_empty());
    }

    #[test]
    fn invalid_git_package_json_still_errors() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("package.json"), "{").unwrap();

        let err = read_git_package_manifest("broken", temp.path(), "clone", None).unwrap_err();

        assert!(
            err.to_string().contains("parse package.json in clone"),
            "{err}"
        );
    }

    #[test]
    fn missing_git_subpath_still_errors() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("packages/missing");

        let err = read_git_package_manifest(
            "missing-subpath",
            &missing,
            "clone",
            Some("packages/missing"),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("stat git package root in clone at /packages/missing"),
            "{err}"
        );
    }
}
