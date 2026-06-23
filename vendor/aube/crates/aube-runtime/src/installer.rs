//! Download → verify → extract → atomically publish a Node version
//! into aube's runtime dir.
//!
//! Concurrency: an `xx::fslock` per version serializes competing aube
//! processes; after acquiring the lock the destination is re-checked
//! (the other process may have won). Publishing is staging-dir →
//! `fs::rename`, so the final path only ever appears fully formed —
//! no `incomplete` marker is needed for aube's own installs.

use crate::discover::{InstallOrigin, InstalledNode, validate_install};
use crate::error::Error;
use crate::http::Http;
use crate::paths;
use crate::progress::{DownloadProgress, InstallPhase};
use sha2::Digest;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub(crate) struct DownloadSpec {
    pub(crate) url: String,
    pub(crate) expected_sha256: [u8; 32],
    /// Windows zip vs gz tarball.
    pub(crate) zip: bool,
}

/// Install `version` from `spec` into the runtime dir, returning the
/// validated install. No-op (returns the existing install) when the
/// version is already present and valid.
pub(crate) async fn install(
    http: &Http,
    version: &node_semver::Version,
    spec: &DownloadSpec,
    progress: &dyn DownloadProgress,
) -> Result<InstalledNode, Error> {
    let runtime_dir = paths::runtime_dir().ok_or_else(|| {
        Error::io(
            "locate the aube runtime dir",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"),
        )
    })?;
    let dest = runtime_dir.join(version.to_string());
    if let Some(existing) = validate_install(&dest, version.clone(), InstallOrigin::Aube) {
        return Ok(existing);
    }

    let locks = paths::locks_dir().expect("runtime_dir implies locks_dir");
    std::fs::create_dir_all(&locks)
        .map_err(|e| Error::io(format!("create {}", locks.display()), e))?;
    let lock_path = locks.join(format!("{version}.lock"));
    // fslock is blocking; hold it on a blocking thread for the whole
    // install. The closure below runs the async download via a handle
    // back into the runtime.
    let lock = tokio::task::spawn_blocking(move || xx::fslock::FSLock::new(&lock_path).lock())
        .await
        .map_err(|e| {
            Error::io(
                "acquire runtime install lock",
                std::io::Error::other(e.to_string()),
            )
        })?
        .map_err(|e| {
            Error::io(
                "acquire runtime install lock",
                std::io::Error::other(e.to_string()),
            )
        })?;

    // Re-check under the lock: another process may have finished the
    // install while we waited.
    if let Some(existing) = validate_install(&dest, version.clone(), InstallOrigin::Aube) {
        drop(lock);
        return Ok(existing);
    }

    gc_stale_temp_dirs();

    let result = download_verify_extract(http, version, spec, &dest, progress).await;
    drop(lock);
    result
}

async fn download_verify_extract(
    http: &Http,
    version: &node_semver::Version,
    spec: &DownloadSpec,
    dest: &Path,
    progress: &dyn DownloadProgress,
) -> Result<InstalledNode, Error> {
    let downloads = paths::downloads_dir().expect("runtime_dir implies downloads_dir");
    let staging_root = paths::staging_dir().expect("runtime_dir implies staging_dir");
    std::fs::create_dir_all(&downloads)
        .map_err(|e| Error::io(format!("create {}", downloads.display()), e))?;
    std::fs::create_dir_all(&staging_root)
        .map_err(|e| Error::io(format!("create {}", staging_root.display()), e))?;

    // Download, hashing incrementally.
    progress.on_phase(Some(version), InstallPhase::Downloading);
    let archive_name = spec
        .url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("node-archive");
    let archive_path = downloads.join(format!("{archive_name}.{}", std::process::id()));
    let actual = stream_to_file(http, &spec.url, &archive_path, progress).await?;

    progress.on_phase(Some(version), InstallPhase::Verifying);
    if actual != spec.expected_sha256 {
        let _ = std::fs::remove_file(&archive_path);
        return Err(Error::ChecksumMismatch {
            url: spec.url.clone(),
            expected: hex::encode(spec.expected_sha256),
            actual: hex::encode(actual),
        });
    }

    // Extract into staging, then atomically publish.
    progress.on_phase(Some(version), InstallPhase::Extracting);
    let staging = staging_root.join(format!("{version}.{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .map_err(|e| Error::io(format!("create {}", staging.display()), e))?;
    let extract_archive = archive_path.clone();
    let extract_staging = staging.clone();
    let zip = spec.zip;
    let extract_result = tokio::task::spawn_blocking(move || {
        crate::extract::extract_archive(&extract_archive, &extract_staging, zip, true)
    })
    .await
    .map_err(|e| Error::ExtractFailed {
        reason: e.to_string(),
    })?;
    let _ = std::fs::remove_file(&archive_path);
    if let Err(e) = extract_result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    match std::fs::rename(&staging, dest) {
        Ok(()) => {}
        Err(rename_err) => {
            // A concurrent winner (or an AlreadyExists race on
            // platforms where rename-over-dir fails) is success if the
            // destination now validates.
            let _ = std::fs::remove_dir_all(&staging);
            if validate_install(dest, version.clone(), InstallOrigin::Aube).is_none() {
                return Err(Error::io(
                    format!("publish {} into {}", version, dest.display()),
                    rename_err,
                ));
            }
        }
    }
    progress.on_done();

    validate_install(dest, version.clone(), InstallOrigin::Aube).ok_or_else(|| {
        Error::ExtractFailed {
            reason: format!(
                "extracted archive did not produce a usable node at {}",
                dest.display()
            ),
        }
    })
}

/// Stream `url` to `path`, returning the SHA-256 of the bytes
/// written.
pub(crate) async fn stream_to_file(
    http: &Http,
    url: &str,
    path: &PathBuf,
    progress: &dyn DownloadProgress,
) -> Result<[u8; 32], Error> {
    let resp = http.get(url, None, None, true).await?;
    let mut body = resp.body.ok_or_else(|| Error::DownloadFailed {
        url: url.to_string(),
        reason: "unexpected empty response".to_string(),
    })?;
    progress.on_download_start(body.content_length());
    let mut hasher = sha2::Sha256::new();
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| Error::io(format!("create {}", path.display()), e))?;
    loop {
        match body.chunk().await {
            Ok(Some(chunk)) => {
                hasher.update(&chunk);
                progress.on_download_chunk(chunk.len() as u64);
                file.write_all(&chunk)
                    .await
                    .map_err(|e| Error::io(format!("write {}", path.display()), e))?;
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(path).await;
                return Err(Error::DownloadFailed {
                    url: url.to_string(),
                    reason: e.to_string(),
                });
            }
        }
    }
    file.flush()
        .await
        .map_err(|e| Error::io(format!("flush {}", path.display()), e))?;
    Ok(hasher.finalize().into())
}

/// Best-effort cleanup of crash debris in `.downloads/` and `.tmp/`
/// older than 24h. Runs under the per-version lock, so concurrent
/// installs of *other* versions could in principle race a GC of their
/// live temp files — the age threshold makes that practically
/// impossible (a 24h-old in-flight download is dead).
fn gc_stale_temp_dirs() {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    for dir in [paths::downloads_dir(), paths::staging_dir()]
        .into_iter()
        .flatten()
    {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if modified < cutoff {
                let path = entry.path();
                tracing::debug!(path = %path.display(), "removing stale runtime temp entry");
                if meta.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}
