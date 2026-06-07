//! PM provisioning: resolve a [`PmPin`] to an on-disk, runnable package manager,
//! reusing nub's existing download + integrity + extract machinery.
//!
//! The store is **version-addressed**, mirroring [`provision_node`]:
//! `<store_root>/pm/<pm>/<version>/` is the install root, and its presence (the
//! `package/` dir with the resolved bin) is the cache-hit signal — a hit is
//! trusted, the same posture as `version_dir_has_node`. This is NOT a
//! content-addressed (by-digest) store; integrity is verified once, at install,
//! BEFORE extraction.
//!
//! [`provision_pm`] returns only the path + version. The caller execs the bin
//! under the *project's* resolved/provisioned Node (`discover_node` /
//! `discover_or_provision_node`) — never the shell's `node` — so a pinned PM never
//! runs against an unpinned runtime.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use super::registry::{self, VersionDist};
use super::resolve::PmPin;
use crate::pm::extract::extract_tgz;
use crate::version_management::download;

/// A provisioned package manager: the runnable bin and the concrete version it
/// resolved to (the spec may have been a range / dist-tag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedPm {
    pub bin: PathBuf,
    pub version: String,
}

/// Download + verify + extract the pinned package manager into nub's store,
/// returning its runnable bin. Flow mirrors [`provision_node`]:
///   1. strip any Corepack `+sha512.…` hash suffix from the pinned version,
///   2. resolve the spec to a concrete version + dist via the registry,
///   3. cache-check `<store_root>/pm/<pm>/<version>/` (offline hit, silent),
///   4. download into a sibling temp dir (cleaned up by the [`WorkGuard`]),
///   5. verify integrity BEFORE extraction (executables landing on disk),
///   6. extract the `.tgz` and atomically `rename` into place,
///   7. uv-style `Installing…` / `✓ Installed…` on STDERR.
///
/// `pin.version` must be present — a [`PmPin`] with no version can't be
/// provisioned (the caller resolves the spec from a lockfile / `packageManager`
/// before reaching here). The returned bin is `<version>/package/<bin_subpath>`.
pub fn provision_pm(pin: &PmPin, store_root: &Path) -> Result<ProvisionedPm> {
    let pm = pin.pm;
    let spec = pin
        .version
        .as_deref()
        .map(strip_hash_suffix)
        .with_context(|| format!("no version to provision for {pm}"))?;

    let base = registry::registry_base(store_root);
    let dist = registry::resolve_version(&base, &pm.to_string(), spec)?;

    let pm_store = store_root.join("pm").join(pm.to_string());
    let final_dir = pm_store.join(&dist.version);
    let bin = final_dir.join("package").join(&dist.bin_subpath);
    if bin.is_file() {
        return Ok(ProvisionedPm {
            bin,
            version: dist.version,
        }); // cache hit — silent
    }

    install(pm, &dist, &pm_store, &final_dir)?;
    Ok(ProvisionedPm {
        bin,
        version: dist.version,
    })
}

/// The download/verify/extract/place body — factored out so [`provision_pm`]'s
/// happy path reads as a flat sequence. Modeled on [`provision_node`]'s skeleton
/// (deliberately re-stated rather than abstracted: two artifact kinds — Node
/// tarballs and PM `.tgz`s — would make a generic `Provisioner` trait pure
/// indirection).
fn install(
    pm: super::Pm,
    dist: &VersionDist,
    pm_store: &Path,
    final_dir: &Path,
) -> Result<()> {
    // Sibling temp dir on the same filesystem → final placement is an atomic
    // rename. The guard cleans it up on every exit path.
    let work = pm_store.join(format!(".tmp-{}-{}", dist.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let _guard = WorkGuard(work.clone());

    let started = Instant::now();
    let tarball = work.join("package.tgz");
    let mut announced = false;
    download::download_to_file(&dist.tarball, &tarball, |_done, total| {
        if !announced {
            announced = true;
            match total {
                Some(t) => eprintln!("Installing {pm} {} ({} MB)...", dist.version, t / 1_000_000),
                None => eprintln!("Installing {pm} {}...", dist.version),
            }
        }
    })
    .with_context(|| format!("downloading {pm} {}", dist.version))?;

    // Verify BEFORE extracting.
    registry::verify_integrity(&tarball, &dist.integrity)
        .with_context(|| format!("verifying {pm} {}", dist.version))?;

    // Extract into a clean sibling so `staging`'s only child is the tarball's
    // `package/` top dir; renaming `staging` into place makes the install root
    // `<final_dir>/package/…` (the bin path callers expect).
    let staging = work.join("staging");
    extract_tgz(&tarball, &staging)?;

    // Atomic place. If a concurrent run already installed it, keep theirs.
    if !final_dir.join("package").is_dir() {
        std::fs::create_dir_all(pm_store).ok();
        if let Err(e) = std::fs::rename(&staging, final_dir) {
            if !final_dir.join("package").is_dir() {
                return Err(e).with_context(|| {
                    format!("installing {pm} {} into {}", dist.version, final_dir.display())
                });
            }
        }
    }

    eprintln!(
        "✓ Installed {pm} {} in {:.1}s",
        dist.version,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Strip the Corepack hash suffix (`10.0.0+sha512.abc…`) from a `packageManager`
/// version so the bare `X.Y.Z` (or range / dist-tag) reaches the registry. The
/// integrity gate is the registry's `dist.integrity`, not this suffix — nub does
/// not honor `COREPACK_INTEGRITY_KEYS` (out of scope).
fn strip_hash_suffix(version: &str) -> &str {
    version.split_once('+').map_or(version, |(v, _)| v)
}

/// Best-effort cleanup of the temp work dir on any return path (the same guard
/// shape `provision_node` uses; deliberately not shared — it's three lines and
/// lives next to the one flow that owns it).
struct WorkGuard(PathBuf);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::Pm;

    #[test]
    fn strips_the_corepack_hash_suffix_only() {
        assert_eq!(strip_hash_suffix("10.0.0+sha512.abc123"), "10.0.0");
        assert_eq!(strip_hash_suffix("10.0.0"), "10.0.0", "no suffix is untouched");
        assert_eq!(strip_hash_suffix("^9"), "^9", "a range is untouched");
    }

    /// Real-network e2e: provision pnpm@10.0.0 into a temp store, run the bin under
    /// THIS host's Node, and confirm `--version` prints 10.0.0; a second call is a
    /// silent cache hit returning the identical path. `#[ignore]` — network +
    /// downloads a real PM tarball.
    ///   cargo test -p nub-core --lib pm::provision::tests::provision_real -- --ignored
    #[test]
    #[ignore = "network: provisions real pnpm@10.0.0 and execs it under host Node"]
    fn provision_real_pnpm_and_run_under_node() {
        let store = std::env::temp_dir().join(format!("nub-pm-prov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let pin = PmPin {
            pm: Pm::Pnpm,
            version: Some("10.0.0".to_string()),
        };

        let prov = provision_pm(&pin, &store).expect("provision pnpm");
        assert_eq!(prov.version, "10.0.0");
        assert!(prov.bin.is_file(), "the resolved bin must be on disk");

        // Exec under the project's resolved Node (the contract: never the bare
        // shell `node`). Discovery from the temp store's cwd has no pin, so it uses
        // PATH node here — sufficient to prove the provisioned bin runs.
        let node = crate::node::discovery::discover_node(&store)
            .expect("a node to run pnpm under");
        let out = std::process::Command::new(&node.path)
            .arg(&prov.bin)
            .arg("--version")
            .output()
            .expect("run pnpm --version");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "10.0.0");

        // Second call: silent cache hit, identical path.
        let again = provision_pm(&pin, &store).expect("cache hit");
        assert_eq!(again, prov);
        let _ = std::fs::remove_dir_all(&store);
    }
}
