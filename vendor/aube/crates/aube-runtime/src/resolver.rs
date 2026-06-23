//! The resolution state machine. Hot-path guarantee: when a
//! satisfying Node is already on PATH or installed (aube or mise),
//! resolution touches the network never and spawns at most one
//! memoized `node --version`.

use crate::discover::{self, InstallOrigin, InstalledNode};
use crate::error::Error;
use crate::http::Http;
use crate::index;
use crate::installer::{self, DownloadSpec};
use crate::mise;
use crate::platform::{Platform, artifact_filename, artifact_top_dir};
use crate::progress::DownloadProgress;
use crate::shasums::{self, sha256_from_sri, sri_sha256};
use crate::spec::{NodeRequest, NodeSpec};
use crate::{InstallerMode, PinnedNode, PinnedVariant, RuntimeConfig};
use aube_manifest::OnFail;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How a resolution was satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFrom {
    /// The `node` already on PATH satisfies the request — no PATH
    /// manipulation needed.
    PathEnv,
    /// An existing install (aube's runtime dir or mise's installs).
    Installed(InstallOrigin),
    /// Installed during this resolution.
    FreshInstall(InstallOrigin),
}

/// A successfully resolved runtime.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub version: node_semver::Version,
    /// Directory to prepend to PATH. `None` for [`ResolvedFrom::PathEnv`].
    pub bin_dir: Option<PathBuf>,
    pub node_bin: PathBuf,
    pub from: ResolvedFrom,
    /// Populated when this resolution hit the network index — lets the
    /// caller record/refresh the lockfile pin without a second
    /// SHASUMS round-trip.
    pub fresh_pin: Option<PinnedNode>,
}

pub struct NodeRuntime {
    pub(crate) cfg: RuntimeConfig,
    pub(crate) http: Http,
    memo: tokio::sync::Mutex<BTreeMap<String, Option<Resolution>>>,
}

impl NodeRuntime {
    pub fn new(cfg: RuntimeConfig) -> Self {
        let http = Http::new(cfg.retries);
        NodeRuntime {
            cfg,
            http,
            memo: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Resolve `req`, preferring the lockfile `pinned` version when
    /// present.
    ///
    /// `Ok(None)` means "leave the environment alone": the request
    /// couldn't be satisfied locally and the `onFail` policy
    /// (`ignore`/`warn`) says to keep running on whatever node PATH
    /// provides. `Ok(Some(_))` is a concrete runtime to put on PATH
    /// (or, for [`ResolvedFrom::PathEnv`], to leave as-is).
    pub async fn resolve(
        &self,
        req: &NodeRequest,
        pinned: Option<&PinnedNode>,
        progress: &dyn DownloadProgress,
    ) -> Result<Option<Resolution>, Error> {
        let memo_key = match pinned {
            Some(p) => format!("pin:{}", p.version),
            None => format!("spec:{}", req.raw),
        };
        if let Some(hit) = self.memo.lock().await.get(&memo_key) {
            return Ok(hit.clone());
        }
        let result = self.resolve_uncached(req, pinned, progress).await?;
        self.memo.lock().await.insert(memo_key, result.clone());
        Ok(result)
    }

    async fn resolve_uncached(
        &self,
        req: &NodeRequest,
        pinned: Option<&PinnedNode>,
        progress: &dyn DownloadProgress,
    ) -> Result<Option<Resolution>, Error> {
        // The lockfile pin wins over the range: reproducibility.
        let target = match pinned {
            Some(p) => NodeSpec::Exact(p.version.clone()),
            None => req.spec.clone(),
        };

        // Zero-network fast paths. Lts/Latest/codename targets skip
        // them — satisfaction is unknowable without the index.
        if let Some(resolution) = local_resolution(&target) {
            return Ok(Some(resolution));
        }

        // Locally unsatisfiable: apply policy before touching the
        // network — but only for specs whose satisfaction is locally
        // decidable. Alias specs (`lts`, `latest`, codenames) need the
        // index first under *every* policy: the installed node may
        // well BE the latest LTS, and warning or erroring without
        // checking would be a false positive. Policy gates runtime
        // downloads, not metadata fetches.
        let locally_decidable = matches!(target, NodeSpec::Exact(_) | NodeSpec::Range(_));
        if locally_decidable {
            match req.on_fail {
                OnFail::Ignore => return Ok(None),
                OnFail::Warn => {
                    warn_version_mismatch(req);
                    return Ok(None);
                }
                OnFail::Error => return Err(self.unsatisfied(req)),
                OnFail::Download => {}
            }
        }

        // Network: pin the spec to an exact version.
        progress.on_phase(None, crate::progress::InstallPhase::Resolving);
        let platform = Platform::current()?;
        let (version, fresh_pin) = match pinned {
            Some(p) => (p.version.clone(), None),
            None => {
                let selected = match index::load_index(&self.http, &self.cfg).await {
                    Ok(entries) => index::select(&entries, &target, &platform)
                        .map(|e| e.version.clone())
                        .ok_or_else(|| Error::NoMatchingVersion {
                            requested: req.raw.clone(),
                            platform_note: format!(" with a build for {}", platform.label()),
                        }),
                    Err(e) => Err(e),
                };
                match selected {
                    Ok(v) => (v, None),
                    // Under warn/ignore the requirement is advisory —
                    // an unreachable index must not block the command.
                    Err(_) if req.on_fail == OnFail::Ignore => return Ok(None),
                    Err(e) if req.on_fail == OnFail::Warn => {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_RUNTIME_VERSION_MISMATCH,
                            requested = %req.raw,
                            source = req.source.label(),
                            error = %e,
                            "could not verify the project's runtime requirement; continuing on the active Node.js"
                        );
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        // The exact version may already be present even though the
        // range check above couldn't run (alias specs) or the pin
        // differs from what PATH carries.
        let exact = NodeSpec::Exact(version.clone());
        if let Some(resolution) = local_resolution(&exact) {
            return Ok(Some(resolution));
        }
        // Alias specs reach their policy here, after the index turned
        // them into a concrete version (a confirmed mismatch, not a
        // guess).
        match req.on_fail {
            OnFail::Ignore => return Ok(None),
            OnFail::Warn => {
                warn_version_mismatch(req);
                return Ok(None);
            }
            OnFail::Error => return Err(self.unsatisfied(req)),
            OnFail::Download => {}
        }

        // Build the download spec: lockfile variant when available,
        // live SHASUMS otherwise.
        let artifact_base = self.cfg.artifact_base(&platform);
        let pinned_variant = pinned
            .and_then(|p| p.variant_for(&platform.os, &platform.cpu, platform.libc.as_deref()));
        let (download, fresh_pin) = match pinned_variant {
            Some(v) => {
                let expected =
                    sha256_from_sri(&v.integrity_sri).ok_or_else(|| Error::ChecksumMismatch {
                        url: v.url.clone(),
                        expected: v.integrity_sri.clone(),
                        actual: "<unparseable lockfile integrity>".to_string(),
                    })?;
                (
                    DownloadSpec {
                        url: v.url.clone(),
                        expected_sha256: expected,
                        zip: v.archive == "zip",
                    },
                    fresh_pin,
                )
            }
            None => {
                if pinned.is_some() {
                    // Lockfile written before this platform was
                    // supported — verify against live SHASUMS instead
                    // and let the caller refresh the pin.
                    tracing::warn!(
                        version = %version,
                        platform = %platform.label(),
                        "lockfile runtime pin has no variant for this platform; using live checksums"
                    );
                }
                let sums =
                    shasums::load_shasums(&self.http, &self.cfg, &artifact_base, &version).await?;
                let filename = artifact_filename(&version, &platform);
                let digest = sums.for_file(&filename).copied().ok_or_else(|| {
                    Error::UnsupportedPlatform {
                        platform: platform.label(),
                    }
                })?;
                let pin = self.build_full_pin(&version).await.unwrap_or_else(|e| {
                    tracing::debug!(error = %e, "could not build full runtime pin");
                    PinnedNode {
                        version: version.clone(),
                        variants: Vec::new(),
                    }
                });
                (
                    DownloadSpec {
                        url: format!("{artifact_base}/v{version}/{filename}"),
                        expected_sha256: digest,
                        zip: platform.os == "win32",
                    },
                    Some(pin),
                )
            }
        };

        // Install, honoring the delegation mode.
        let installed = self.install(&version, &download, progress).await?;
        Ok(Some(Resolution {
            version: installed.version.clone(),
            bin_dir: Some(installed.bin_dir.clone()),
            node_bin: installed.node_bin.clone(),
            from: ResolvedFrom::FreshInstall(installed.origin),
            fresh_pin,
        }))
    }

    async fn install(
        &self,
        version: &node_semver::Version,
        download: &DownloadSpec,
        progress: &dyn DownloadProgress,
    ) -> Result<InstalledNode, Error> {
        match self.cfg.installer {
            InstallerMode::Aube => {
                installer::install(&self.http, version, download, progress).await
            }
            InstallerMode::Mise => {
                let Some(mise_bin) = mise::mise_on_path() else {
                    return Err(Error::MiseInstallFailed {
                        version: format!("node@{version}"),
                        reason: "runtimeInstaller=mise but mise is not on PATH".to_string(),
                    });
                };
                mise::install_via_mise(&mise_bin, version, progress).await
            }
            InstallerMode::Auto => match mise::mise_on_path() {
                Some(mise_bin) => {
                    match mise::install_via_mise(&mise_bin, version, progress).await {
                        Ok(node) => Ok(node),
                        Err(e) => {
                            tracing::warn!(
                                code = aube_codes::warnings::WARN_AUBE_RUNTIME_MISE_FALLBACK,
                                error = %e,
                                "mise failed to install the runtime; falling back to aube's own download"
                            );
                            installer::install(&self.http, version, download, progress).await
                        }
                    }
                }
                None => installer::install(&self.http, version, download, progress).await,
            },
        }
    }

    fn unsatisfied(&self, req: &NodeRequest) -> Error {
        let current = discover::probe_path_node()
            .map(|(v, _)| format!(" (PATH provides {v})"))
            .unwrap_or_else(|| " (no node on PATH)".to_string());
        Error::VersionUnsatisfied {
            requested: req.raw.clone(),
            hint: format!(
                "{current}; required by {} at {}",
                req.source.label(),
                req.origin.display()
            ),
        }
    }

    /// Resolve `spec` to an exact version plus the full per-platform
    /// artifact set — the lockfile-pin path. Always network-backed
    /// (through the disk caches).
    pub async fn resolve_for_lockfile(&self, spec: &NodeSpec) -> Result<PinnedNode, Error> {
        let platform = Platform::current()?;
        let entries = index::load_index(&self.http, &self.cfg).await?;
        let entry =
            index::select(&entries, spec, &platform).ok_or_else(|| Error::NoMatchingVersion {
                requested: spec.display(),
                platform_note: String::new(),
            })?;
        let version = entry.version.clone();
        self.build_full_pin(&version).await
    }

    /// Build a full pin (all platforms) from SHASUMS data: the
    /// configured mirror's checksums, plus — when running against the
    /// default official mirror — unofficial-builds' musl checksums,
    /// best-effort (older releases have no musl builds).
    async fn build_full_pin(&self, version: &node_semver::Version) -> Result<PinnedNode, Error> {
        let base = self.cfg.mirror_base();
        let sums = shasums::load_shasums(&self.http, &self.cfg, &base, version).await?;
        let mut variants = variants_from_shasums(&base, version, sums.iter());
        if self.cfg.mirror.is_none() {
            let musl_base = crate::UNOFFICIAL_BASE;
            match shasums::load_shasums(&self.http, &self.cfg, musl_base, version).await {
                Ok(musl_sums) => {
                    variants.extend(
                        variants_from_shasums(musl_base, version, musl_sums.iter())
                            .into_iter()
                            .filter(|v| v.libc.as_deref() == Some("musl")),
                    );
                }
                Err(e) => {
                    tracing::debug!(error = %e, "no musl builds recorded for v{version}");
                }
            }
        }
        Ok(PinnedNode {
            version: version.clone(),
            variants,
        })
    }
}

fn warn_version_mismatch(req: &NodeRequest) {
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_RUNTIME_VERSION_MISMATCH,
        requested = %req.raw,
        source = req.source.label(),
        "the active Node.js does not satisfy the project's runtime requirement"
    );
}

/// Zero-network resolution: PATH probe, then installed scan. Only
/// meaningful for `Exact` / `Range` targets.
fn local_resolution(target: &NodeSpec) -> Option<Resolution> {
    if let Some((version, node_bin)) = discover::probe_path_node()
        && target.satisfied_by(&version) == Some(true)
    {
        return Some(Resolution {
            version,
            bin_dir: None,
            node_bin,
            from: ResolvedFrom::PathEnv,
            fresh_pin: None,
        });
    }
    let best = discover::list_installed()
        .into_iter()
        .filter(|n| target.satisfied_by(&n.version) == Some(true))
        .max_by(|a, b| a.version.cmp(&b.version))?;
    Some(Resolution {
        version: best.version.clone(),
        bin_dir: Some(best.bin_dir.clone()),
        node_bin: best.node_bin.clone(),
        from: ResolvedFrom::Installed(best.origin),
        fresh_pin: None,
    })
}

/// Map SHASUMS entries (`<hex>  node-v{V}-{os}-{arch}[-musl].{ext}`)
/// onto lockfile variants, mirroring pnpm's `readNodeAssetsFromMirror`:
/// `win` → `win32`, bin paths per OS, `prefix` set for zips.
fn variants_from_shasums<'a>(
    base: &str,
    version: &node_semver::Version,
    entries: impl Iterator<Item = (&'a String, &'a [u8; 32])>,
) -> Vec<PinnedVariant> {
    let prefix = format!("node-v{version}-");
    let mut out = Vec::new();
    for (filename, digest) in entries {
        let Some(rest) = filename.strip_prefix(&prefix) else {
            continue;
        };
        let (slug, ext) = if let Some(s) = rest.strip_suffix(".tar.gz") {
            (s, "tar.gz")
        } else if let Some(s) = rest.strip_suffix(".zip") {
            (s, "zip")
        } else {
            continue;
        };
        let (slug, musl) = match slug.strip_suffix("-musl") {
            Some(s) => (s, true),
            None => (slug, false),
        };
        let Some((os_raw, cpu)) = slug.split_once('-') else {
            continue;
        };
        // Only the canonical platform pairs; skip exotic artifacts
        // (headers, pkg, 7z multi-dash names fall out naturally via
        // the extension filter, `win-x64-7z` via the split shape).
        if cpu.contains('-') {
            continue;
        }
        let os = match os_raw {
            "win" => "win32",
            "osx" | "darwin" => "darwin",
            "linux" => "linux",
            "aix" => "aix",
            _ => continue,
        };
        let bin: BTreeMap<String, String> = if os == "win32" {
            [("node".to_string(), "node.exe".to_string())].into()
        } else {
            [("node".to_string(), "bin/node".to_string())].into()
        };
        out.push(PinnedVariant {
            os: os.to_string(),
            cpu: cpu.to_string(),
            libc: musl.then(|| "musl".to_string()),
            archive: if ext == "zip" { "zip" } else { "tarball" }.to_string(),
            url: format!("{base}/v{version}/{filename}"),
            integrity_sri: sri_sha256(digest),
            bin,
            prefix: (ext == "zip").then(|| {
                let plat = Platform {
                    os: os.to_string(),
                    cpu: cpu.to_string(),
                    libc: musl.then(|| "musl".to_string()),
                };
                artifact_top_dir(version, &plat)
            }),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shasums_variant_mapping() {
        let version: node_semver::Version = "24.4.1".parse().unwrap();
        let entries: Vec<(String, [u8; 32])> = vec![
            ("node-v24.4.1-darwin-arm64.tar.gz".into(), [1; 32]),
            ("node-v24.4.1-linux-x64.tar.gz".into(), [2; 32]),
            ("node-v24.4.1-linux-x64-musl.tar.gz".into(), [3; 32]),
            ("node-v24.4.1-win-x64.zip".into(), [4; 32]),
            ("node-v24.4.1-headers.tar.gz".into(), [5; 32]),
            ("node-v24.4.1.pkg".into(), [6; 32]),
            ("node-v24.4.1-win-x64.7z".into(), [7; 32]),
            ("node-v24.4.1-darwin-arm64.tar.xz".into(), [8; 32]),
        ];
        let variants = variants_from_shasums(
            "https://nodejs.org/download/release",
            &version,
            entries.iter().map(|(k, v)| (k, v)),
        );
        let labels: Vec<String> = variants
            .iter()
            .map(|v| {
                format!(
                    "{}-{}{}",
                    v.os,
                    v.cpu,
                    v.libc
                        .as_deref()
                        .map(|l| format!("-{l}"))
                        .unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(
            labels,
            vec!["darwin-arm64", "linux-x64", "linux-x64-musl", "win32-x64"]
        );
        let win = variants.iter().find(|v| v.os == "win32").unwrap();
        assert_eq!(win.archive, "zip");
        assert_eq!(win.prefix.as_deref(), Some("node-v24.4.1-win-x64"));
        assert_eq!(win.bin.get("node").map(String::as_str), Some("node.exe"));
        assert!(win.url.ends_with("/v24.4.1/node-v24.4.1-win-x64.zip"));
        let mac = variants.iter().find(|v| v.os == "darwin").unwrap();
        assert_eq!(mac.archive, "tarball");
        assert_eq!(mac.prefix, None);
        assert!(mac.integrity_sri.starts_with("sha256-"));
    }
}
