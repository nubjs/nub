//! aube self-version management: discovering, downloading, and
//! installing *aube* binaries so a project's `packageManager` /
//! `devEngines.packageManager` pin can re-exec the right version
//! (corepack semantics; pnpm's `managePackageManagerVersions`).
//!
//! Sources, same shape as Node runtimes: mise installs
//! (`installs/aube/<v>/`, binaries at the version root) are reused
//! read-only, and self-downloads come from GitHub release archives
//! (`aube-v{V}-{target-triple}.tar.gz` / `.zip`, binaries at the
//! archive root) into `$XDG_DATA_HOME/aube/self/<v>/`, verified
//! against GitHub's server-computed release asset digests.

use crate::discover::{self, InstallOrigin};
use crate::error::Error;
use crate::http::Http;
use crate::installer::stream_to_file;
use crate::mise;
use crate::progress::{DownloadProgress, InstallPhase};
use crate::{InstallerMode, RuntimeConfig};
use std::path::{Path, PathBuf};

/// Default base for release archives. `AUBE_SELF_DOWNLOAD_BASE`
/// overrides for tests and mirrors; archives live at
/// `{base}/v{V}/aube-v{V}-{triple}.{ext}`.
const RELEASE_BASE: &str = "https://github.com/jdx/aube/releases/download";

/// mise-versions host: CDN-cached, rate-limit-free mirrors of the
/// release version list (`/aube`, plaintext) and GitHub release
/// metadata (`/api/github/repos/jdx/aube/releases/<tag>`, including
/// `assets[].digest`) — the same service mise itself consults before
/// falling back to the GitHub API. `AUBE_VERSIONS_HOST` overrides for
/// tests.
const VERSIONS_HOST: &str = "https://mise-versions.jdx.dev";

/// GitHub releases API fallback for asset digests: GitHub computes a
/// server-side SHA-256 for every release asset (`assets[].digest`,
/// tamper-evident under immutable releases). Consulted when the
/// versions host misses; honors `GITHUB_TOKEN`. `AUBE_SELF_API_BASE`
/// overrides for tests.
const RELEASE_API_BASE: &str = "https://api.github.com/repos/jdx/aube/releases/tags";

/// Endpoint announcing the newest release (one line, bare version).
/// Shared with the update notifier. `AUBE_SELF_VERSION_URL` overrides.
const VERSION_URL: &str = "https://aube.jdx.dev/VERSION";

/// A validated on-disk aube install.
#[derive(Debug, Clone)]
pub struct InstalledAube {
    pub version: node_semver::Version,
    pub install_dir: PathBuf,
    /// The `aube` executable. `aubr` / `aubx` siblings live next to it.
    pub exe: PathBuf,
    pub origin: InstallOrigin,
}

/// aube's own versions dir (`$XDG_DATA_HOME/aube/self`).
/// `AUBE_SELF_DIR` overrides for tests.
pub fn self_dir() -> Option<PathBuf> {
    if let Some(dir) = aube_util::env::embedder_env("SELF_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("aube/self"));
    }
    let data_home = aube_util::env::xdg_data_home()
        .or_else(|| aube_util::env::home_dir().map(|h| h.join(".local/share")))?;
    Some(data_home.join("aube/self"))
}

/// Every valid installed aube across mise's installs dir and aube's
/// self dir. Same collision rule as Node: aube's own copy of a
/// version wins over mise's.
pub fn list_installed_aube() -> Vec<InstalledAube> {
    let mut by_version: std::collections::BTreeMap<node_semver::Version, InstalledAube> =
        Default::default();
    if let Some(dir) = discover::mise_tool_installs_dir("aube") {
        for install in scan_aube_dir(&dir, InstallOrigin::Mise) {
            by_version.insert(install.version.clone(), install);
        }
    }
    if let Some(dir) = self_dir() {
        for install in scan_aube_dir(&dir, InstallOrigin::Aube) {
            by_version.insert(install.version.clone(), install);
        }
    }
    by_version.into_values().collect()
}

/// Look up one exact installed version (mise first, then self dir —
/// the self-dir copy wins, mirroring `list_installed_aube`).
pub fn find_installed_aube(version: &node_semver::Version) -> Option<InstalledAube> {
    let from_self = self_dir()
        .map(|d| d.join(version.to_string()))
        .and_then(|d| validate_aube_install(&d, version.clone(), InstallOrigin::Aube));
    from_self.or_else(|| {
        discover::mise_tool_installs_dir("aube")
            .map(|d| d.join(version.to_string()))
            .and_then(|d| validate_aube_install(&d, version.clone(), InstallOrigin::Mise))
    })
}

fn scan_aube_dir(root: &Path, origin: InstallOrigin) -> Vec<InstalledAube> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            // Skips mise's alias symlinks (`1`, `1.18`, `latest`).
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(version) = node_semver::Version::parse(name.trim_start_matches('v')) else {
            continue;
        };
        if let Some(install) = validate_aube_install(&path, version, origin) {
            out.push(install);
        }
    }
    out
}

/// Validate a version dir: no `incomplete` marker (mise's in-progress
/// signal), and the `aube` executable present at the root or under
/// `bin/` (mise and the release archives use the root; `bin/` covers
/// alternative packagings).
fn validate_aube_install(
    dir: &Path,
    version: node_semver::Version,
    origin: InstallOrigin,
) -> Option<InstalledAube> {
    if dir.join("incomplete").exists() {
        return None;
    }
    let exe_name = if cfg!(windows) { "aube.exe" } else { "aube" };
    let exe = [dir.join(exe_name), dir.join("bin").join(exe_name)]
        .into_iter()
        .find(|p| discover::is_executable_file(p))?;
    Some(InstalledAube {
        version,
        install_dir: dir.to_path_buf(),
        exe,
        origin,
    })
}

/// The release-archive target triple for the host. aube publishes:
/// `aarch64-apple-darwin`, `{x86_64,aarch64}-unknown-linux-{gnu,musl}`,
/// `{x86_64,aarch64}-pc-windows-msvc`. Hosts without a published build
/// (e.g. Intel macOS, or any FreeBSD arch) get an
/// [`Error::NoReleaseBuild`] pointing at mise / the system package
/// manager.
pub fn release_target_triple() -> Result<String, Error> {
    let os = std::env::consts::OS;
    let raw_arch = std::env::consts::ARCH;

    // FreeBSD has no published aube archive on any architecture, so
    // short-circuit before the arch whitelist — otherwise a non-x86/arm
    // FreeBSD host would fall into the arch reject and miss the
    // aube-specific guidance. `Platform::current()` still recognizes
    // FreeBSD (see platform.rs), so Node installs keep working against
    // system/mise; only the self-updater is unavailable here.
    if os == "freebsd" {
        return Err(Error::NoReleaseBuild {
            platform: format!("freebsd-{raw_arch}"),
        });
    }

    let arch = match raw_arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            return Err(Error::NoReleaseBuild {
                platform: format!("{os}-{other}"),
            });
        }
    };
    let triple = match os {
        "macos" => {
            if arch != "aarch64" {
                return Err(Error::NoReleaseBuild {
                    platform: "macos-x86_64".to_string(),
                });
            }
            format!("{arch}-apple-darwin")
        }
        "linux" => {
            let libc = if crate::Platform::current()?.libc.as_deref() == Some("musl") {
                "musl"
            } else {
                "gnu"
            };
            format!("{arch}-unknown-linux-{libc}")
        }
        "windows" => format!("{arch}-pc-windows-msvc"),
        other => {
            return Err(Error::NoReleaseBuild {
                platform: format!("{other}-{arch}"),
            });
        }
    };
    Ok(triple)
}

fn release_base() -> String {
    aube_util::env::embedder_env("SELF_DOWNLOAD_BASE")
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| RELEASE_BASE.to_string())
}

fn versions_host() -> String {
    aube_util::env::embedder_env("VERSIONS_HOST")
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| VERSIONS_HOST.to_string())
}

/// Every published aube version (for resolving range pins to the best
/// satisfying release). Primary source is the versions host's
/// plaintext list — CDN-cached, no rate limits; the
/// `aube.jdx.dev/VERSION` latest-only announcement is the fallback,
/// degrading range resolution to "newest release" rather than failing.
pub async fn available_aube_versions(retries: u32) -> Result<Vec<node_semver::Version>, Error> {
    let http = Http::new(retries);
    let list_url = format!("{}/aube", versions_host());
    match fetch_text(&http, &list_url).await {
        Ok(text) => {
            let versions: Vec<node_semver::Version> = text
                .lines()
                .filter_map(|l| node_semver::Version::parse(l.trim().trim_start_matches('v')).ok())
                .collect();
            if !versions.is_empty() {
                return Ok(versions);
            }
            tracing::debug!(%list_url, "versions host returned an empty list; falling back");
        }
        Err(e) => {
            tracing::debug!(%list_url, error = %e, "versions host unreachable; falling back");
        }
    }
    let url = aube_util::env::embedder_env("SELF_VERSION_URL")
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| VERSION_URL.to_string());
    let text = fetch_text(&http, &url).await?;
    let latest = node_semver::Version::parse(text.trim().trim_start_matches('v')).map_err(|e| {
        Error::DownloadFailed {
            url,
            reason: format!("unparseable version announcement: {e}"),
        }
    })?;
    Ok(vec![latest])
}

async fn fetch_text(http: &Http, url: &str) -> Result<String, Error> {
    let resp = http.get(url, None, None, false).await?;
    let body = resp.body.ok_or_else(|| Error::DownloadFailed {
        url: url.to_string(),
        reason: "unexpected empty response".to_string(),
    })?;
    body.text().await.map_err(|e| Error::DownloadFailed {
        url: url.to_string(),
        reason: e.to_string(),
    })
}

/// Install aube `version`, honoring the installer mode: mise
/// delegation first under `auto`/`mise` (one tool store for mise
/// users), self-download from GitHub releases otherwise.
pub async fn install_aube(
    cfg: &RuntimeConfig,
    version: &node_semver::Version,
    progress: &dyn DownloadProgress,
) -> Result<InstalledAube, Error> {
    if let Some(existing) = find_installed_aube(version) {
        return Ok(existing);
    }
    match cfg.installer {
        InstallerMode::Aube => self_download(cfg, version, progress).await,
        InstallerMode::Mise => {
            let Some(mise_bin) = mise::mise_on_path() else {
                return Err(Error::MiseInstallFailed {
                    version: format!("aube@{version}"),
                    reason: "runtimeInstaller=mise but mise is not on PATH".to_string(),
                });
            };
            delegate_to_mise(&mise_bin, version, progress).await
        }
        InstallerMode::Auto => match mise::mise_on_path() {
            Some(mise_bin) => match delegate_to_mise(&mise_bin, version, progress).await {
                Ok(install) => Ok(install),
                Err(e) => {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_RUNTIME_MISE_FALLBACK,
                        error = %e,
                        "mise failed to install aube; falling back to a release download"
                    );
                    self_download(cfg, version, progress).await
                }
            },
            None => self_download(cfg, version, progress).await,
        },
    }
}

async fn delegate_to_mise(
    mise_bin: &Path,
    version: &node_semver::Version,
    progress: &dyn DownloadProgress,
) -> Result<InstalledAube, Error> {
    mise::install_tool_via_mise(mise_bin, "aube", version, progress).await?;
    discover::mise_tool_installs_dir("aube")
        .map(|d| d.join(version.to_string()))
        .and_then(|d| validate_aube_install(&d, version.clone(), InstallOrigin::Mise))
        .ok_or_else(|| Error::MiseInstallFailed {
            version: format!("aube@{version}"),
            reason: "mise reported success but the install was not found — \
                     if mise uses a custom data dir, export MISE_DATA_DIR so aube sees the same path"
                .to_string(),
        })
}

/// Download a release archive, verify its published `.sha256` when
/// available (older releases predate checksum publishing; those fall
/// back to TLS-only with a debug note), extract — binaries sit at the
/// archive root — and atomically publish.
async fn self_download(
    cfg: &RuntimeConfig,
    version: &node_semver::Version,
    progress: &dyn DownloadProgress,
) -> Result<InstalledAube, Error> {
    let root = self_dir().ok_or_else(|| {
        Error::io(
            "locate the aube self dir",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"),
        )
    })?;
    let dest = root.join(version.to_string());
    let locks = root.join(".locks");
    std::fs::create_dir_all(&locks)
        .map_err(|e| Error::io(format!("create {}", locks.display()), e))?;
    let lock_path = locks.join(format!("{version}.lock"));
    let lock = tokio::task::spawn_blocking(move || xx::fslock::FSLock::new(&lock_path).lock())
        .await
        .map_err(|e| {
            Error::io(
                "acquire self-install lock",
                std::io::Error::other(e.to_string()),
            )
        })?
        .map_err(|e| {
            Error::io(
                "acquire self-install lock",
                std::io::Error::other(e.to_string()),
            )
        })?;
    if let Some(existing) = validate_aube_install(&dest, version.clone(), InstallOrigin::Aube) {
        drop(lock);
        return Ok(existing);
    }

    let triple = release_target_triple()?;
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive_name = format!("aube-v{version}-{triple}.{ext}");
    let url = format!("{}/v{version}/{archive_name}", release_base());
    let http = Http::new(cfg.retries);
    progress.on_phase(Some(version), InstallPhase::Downloading);

    let downloads = root.join(".downloads");
    let staging_root = root.join(".tmp");
    std::fs::create_dir_all(&downloads)
        .map_err(|e| Error::io(format!("create {}", downloads.display()), e))?;
    std::fs::create_dir_all(&staging_root)
        .map_err(|e| Error::io(format!("create {}", staging_root.display()), e))?;
    let archive_path = downloads.join(format!("{archive_name}.{}", std::process::id()));
    let actual = stream_to_file(&http, &url, &archive_path, progress).await?;

    // Expected checksum: GitHub's server-computed asset digest first
    // (covers every release, nothing to publish); a `.sha256` sibling
    // as the fallback for custom mirrors that ship one; TLS-only as
    // the last resort.
    progress.on_phase(Some(version), InstallPhase::Verifying);
    let expected = match fetch_release_digest(&http, version, &archive_name).await {
        Some(digest) => Some(digest),
        None => fetch_published_sha256(&http, &url).await,
    };
    match expected {
        Some(expected) if expected != actual => {
            let _ = std::fs::remove_file(&archive_path);
            drop(lock);
            return Err(Error::ChecksumMismatch {
                url,
                expected: hex::encode(expected),
                actual: hex::encode(actual),
            });
        }
        Some(_) => {}
        None => {
            tracing::debug!(
                %url,
                "no asset digest or .sha256 available for this archive; trusting TLS"
            );
        }
    }

    progress.on_phase(Some(version), InstallPhase::Extracting);
    let staging = staging_root.join(format!("{version}.{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .map_err(|e| Error::io(format!("create {}", staging.display()), e))?;
    let extract_from = archive_path.clone();
    let extract_to = staging.clone();
    let zip = ext == "zip";
    let extract_result = tokio::task::spawn_blocking(move || {
        crate::extract::extract_archive(&extract_from, &extract_to, zip, false)
    })
    .await
    .map_err(|e| Error::ExtractFailed {
        reason: e.to_string(),
    })?;
    let _ = std::fs::remove_file(&archive_path);
    if let Err(e) = extract_result {
        let _ = std::fs::remove_dir_all(&staging);
        drop(lock);
        return Err(e);
    }

    if let Err(rename_err) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_dir_all(&staging);
        if validate_aube_install(&dest, version.clone(), InstallOrigin::Aube).is_none() {
            drop(lock);
            return Err(Error::io(
                format!("publish aube {} into {}", version, dest.display()),
                rename_err,
            ));
        }
    }
    drop(lock);
    progress.on_done();

    validate_aube_install(&dest, version.clone(), InstallOrigin::Aube).ok_or_else(|| {
        Error::ExtractFailed {
            reason: format!(
                "release archive did not produce a usable aube at {}",
                dest.display()
            ),
        }
    })
}

/// Look up the archive's server-computed digest in the GitHub
/// releases API (`assets[].digest`, `"sha256:<hex>"`). Skipped when a
/// custom `AUBE_SELF_DOWNLOAD_BASE` mirror is active without its own
/// `AUBE_SELF_API_BASE` — GitHub's digest describes GitHub's copy,
/// not whatever a mirror chose to serve. `None` on any miss (network,
/// rate limit, unknown tag/asset): the caller falls back rather than
/// failing a download GitHub itself already served over TLS.
async fn fetch_release_digest(
    http: &Http,
    version: &node_semver::Version,
    archive_name: &str,
) -> Option<[u8; 32]> {
    let api_override = aube_util::env::embedder_env("SELF_API_BASE")
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string());
    let host_override = aube_util::env::embedder_env("VERSIONS_HOST").is_some();
    // Custom download mirrors may serve different bytes than GitHub's
    // archives; digests describing GitHub's copies don't apply unless
    // a test override says otherwise.
    if api_override.is_none()
        && !host_override
        && aube_util::env::embedder_env("SELF_DOWNLOAD_BASE").is_some()
    {
        return None;
    }

    // 1. mise-versions proxy: CDN-cached, no rate limits, no token.
    let host_url = format!(
        "{}/api/github/repos/jdx/aube/releases/v{version}",
        versions_host()
    );
    if let Some(digest) =
        digest_from_release_json(http, &host_url, None, version, archive_name).await
    {
        return Some(digest);
    }

    // 2. GitHub API. CI runners and NATed offices share the 60/hr
    // unauthenticated per-IP limit; a token (always present in GitHub
    // Actions) lifts that. Attached only for the real GitHub API host
    // so an `AUBE_SELF_API_BASE` override can never siphon it.
    let url = format!(
        "{}/v{version}",
        api_override.as_deref().unwrap_or(RELEASE_API_BASE)
    );
    let token = url
        .starts_with("https://api.github.com/")
        .then(|| {
            std::env::var("GITHUB_TOKEN")
                .or_else(|_| std::env::var("GH_TOKEN"))
                .ok()
                .filter(|t| !t.trim().is_empty())
        })
        .flatten();
    digest_from_release_json(http, &url, token.as_deref(), version, archive_name).await
}

/// Fetch a GitHub-release-shaped JSON document and pull out
/// `archive_name`'s digest. The returned `tag_name` must echo the
/// requested version — guards against a stale or mis-keyed cache
/// entry on the proxy handing back another release's digests.
async fn digest_from_release_json(
    http: &Http,
    url: &str,
    bearer: Option<&str>,
    version: &node_semver::Version,
    archive_name: &str,
) -> Option<[u8; 32]> {
    let resp = http
        .get_with_bearer(url, None, None, false, bearer)
        .await
        .ok()?;
    let bytes = resp.body?.bytes().await.ok()?;
    let release: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let tag = release.get("tag_name")?.as_str()?;
    if tag != format!("v{version}") {
        tracing::debug!(%url, tag, expected = %format!("v{version}"), "release metadata tag mismatch; ignoring");
        return None;
    }
    let digest = release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(archive_name))?
        .get("digest")?
        .as_str()?;
    parse_sha256_digest(digest)
}

/// Parse GitHub's `"sha256:<hex>"` digest form.
fn parse_sha256_digest(digest: &str) -> Option<[u8; 32]> {
    let hex_part = digest.strip_prefix("sha256:")?;
    let bytes = hex::decode(hex_part).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

/// Fetch `{archive_url}.sha256` and parse the leading hex digest
/// (taiki-e's checksum files are `<hex> *<filename>`). `None` when the
/// asset doesn't exist or doesn't parse — caller decides the policy.
async fn fetch_published_sha256(http: &Http, archive_url: &str) -> Option<[u8; 32]> {
    let url = format!("{archive_url}.sha256");
    let resp = http.get(&url, None, None, false).await.ok()?;
    let text = resp.body?.text().await.ok()?;
    let hex_token = text.split_whitespace().next()?;
    let bytes = hex::decode(hex_token).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fab_aube(root: &Path, version: &str) {
        let dir = root.join(version);
        std::fs::create_dir_all(&dir).unwrap();
        for bin in ["aube", "aubr", "aubx"] {
            let path = dir.join(if cfg!(windows) {
                format!("{bin}.exe")
            } else {
                bin.to_string()
            });
            std::fs::write(&path, "#!/bin/sh\necho fake\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    #[test]
    fn scans_and_validates_aube_installs() {
        let tmp = tempfile::tempdir().unwrap();
        fab_aube(tmp.path(), "1.17.0");
        fab_aube(tmp.path(), "1.18.2");
        fab_aube(tmp.path(), "1.19.0");
        std::fs::write(tmp.path().join("1.19.0/incomplete"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("not-a-version")).unwrap();

        let mut versions: Vec<String> = scan_aube_dir(tmp.path(), InstallOrigin::Mise)
            .into_iter()
            .map(|i| i.version.to_string())
            .collect();
        versions.sort();
        assert_eq!(versions, vec!["1.17.0", "1.18.2"]);
    }

    #[test]
    fn validate_accepts_bin_subdir_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("2.0.0/bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "aube.exe" } else { "aube" });
        std::fs::write(&exe, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let install = validate_aube_install(
            &tmp.path().join("2.0.0"),
            "2.0.0".parse().unwrap(),
            InstallOrigin::Aube,
        )
        .unwrap();
        assert!(install.exe.parent().unwrap().ends_with("bin"));
    }

    #[test]
    fn parses_github_digest_form() {
        let digest = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(parse_sha256_digest(&digest), Some([0xab; 32]));
        assert_eq!(parse_sha256_digest("sha512:abcd"), None);
        assert_eq!(parse_sha256_digest("sha256:nothex"), None);
        assert_eq!(parse_sha256_digest("sha256:abcd"), None); // wrong length
    }

    #[test]
    fn target_triple_is_publishable() {
        // On every platform CI runs, the host triple must map to a
        // name aube actually publishes. Documented exceptions with no
        // published build: Intel macOS, and FreeBSD (any arch).
        match release_target_triple() {
            Ok(t) => {
                assert!(
                    t.contains("apple-darwin")
                        || t.contains("unknown-linux")
                        || t.contains("pc-windows"),
                    "{t}"
                );
            }
            Err(Error::NoReleaseBuild { .. }) => {
                let os = std::env::consts::OS;
                assert!(
                    os == "freebsd" || (os == "macos" && std::env::consts::ARCH == "x86_64"),
                    "unexpected unsupported host: {os}-{}",
                    std::env::consts::ARCH
                );
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
