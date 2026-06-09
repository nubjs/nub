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
///   2. an EXACT pin cache-checks `<store_root>/pm/<pm>/<version>/` before any
///      network — the registry only exists to *resolve* a spec or fetch missing
///      bytes, and an exact, already-installed version needs neither (corepack
///      parity: its run path scans `$COREPACK_HOME/v1/<pm>/` first, so a pinned
///      project runs fully offline; the shim hot path needs the same),
///   3. resolve the spec to a concrete version + dist via the registry — and if
///      the registry is unreachable, a RANGE pin falls back to the best cached
///      satisfying version (announced on stderr; a dist-tag has no offline
///      answer and surfaces the fetch error),
///   4. cache-check the resolved version (silent hit),
///   5. download into a sibling temp dir (cleaned up by the [`WorkGuard`]),
///   6. verify integrity BEFORE extraction (executables landing on disk),
///   7. extract the `.tgz` and atomically `rename` into place,
///   8. uv-style `Installing…` / `✓ Installed…` on STDERR.
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

    let pm_store = store_root.join("pm").join(pm.to_string());

    // 2. Exact pin + cached install → done, zero network. The bin path comes from
    // the cached package's own manifest (same `name`/`bin` shape as the packument).
    if semver::Version::parse(spec).is_ok() {
        if let Some(bin) = cached_bin(&pm_store, spec) {
            return Ok(ProvisionedPm {
                bin,
                version: spec.to_string(),
            }); // cache hit — silent
        }
    }

    let base = registry::registry_base(store_root);
    let dist = match registry::resolve_version(&base, &pm.to_string(), spec) {
        Ok(dist) => dist,
        // 3. Registry unreachable: a range can still resolve against the cache.
        // Exact pins were handled above (and an exact spec parses as a *caret*
        // VersionReq, so it must not reach the range match); dist-tags have no
        // offline meaning. Announced — a stale-vs-fresh divergence from the
        // online behavior should never be silent.
        Err(err) if semver::Version::parse(spec).is_err() => {
            match best_cached_match(&pm_store, spec) {
                Some((version, bin)) => {
                    eprintln!(
                        "nub: registry unreachable; using cached {pm} {version} for \"{spec}\""
                    );
                    return Ok(ProvisionedPm { bin, version });
                }
                None => return Err(err),
            }
        }
        Err(err) => return Err(err),
    };

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

/// The runnable bin of an already-installed `<pm_store>/<version>/`, or `None`
/// when the version isn't cached (or its install is unreadable/incomplete —
/// callers then take the network path, whose installer treats an existing
/// `package/` dir as someone else's completed install).
fn cached_bin(pm_store: &Path, version: &str) -> Option<PathBuf> {
    let pkg_dir = pm_store.join(version).join("package");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pkg_dir.join("package.json")).ok()?).ok()?;
    let bin = pkg_dir.join(registry::bin_subpath(&manifest)?);
    bin.is_file().then_some(bin)
}

/// The highest cached version satisfying a range spec, with its bin. Offline
/// counterpart of [`registry::resolve_dist`]'s range arm: same node-semver→Cargo
/// bridge, same highest-match rule, but over the store's version-named dirs
/// instead of the packument. Non-version dirs (`.tmp-…` work dirs) parse-fail
/// out of the scan.
fn best_cached_match(pm_store: &Path, spec: &str) -> Option<(String, PathBuf)> {
    let req = semver::VersionReq::parse(&registry::normalize_range(spec)).ok()?;
    let versions = std::fs::read_dir(pm_store).ok()?;
    let best = versions
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|name| semver::Version::parse(&name).ok())
        .filter(|v| req.matches(v))
        .max()?
        .to_string();
    let bin = cached_bin(pm_store, &best)?;
    Some((best, bin))
}

/// The download/verify/extract/place body — factored out so [`provision_pm`]'s
/// happy path reads as a flat sequence. Modeled on [`provision_node`]'s skeleton
/// (deliberately re-stated rather than abstracted: two artifact kinds — Node
/// tarballs and PM `.tgz`s — would make a generic `Provisioner` trait pure
/// indirection).
fn install(pm: super::Pm, dist: &VersionDist, pm_store: &Path, final_dir: &Path) -> Result<()> {
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
                    format!(
                        "installing {pm} {} into {}",
                        dist.version,
                        final_dir.display()
                    )
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
        assert_eq!(
            strip_hash_suffix("10.0.0"),
            "10.0.0",
            "no suffix is untouched"
        );
        assert_eq!(strip_hash_suffix("^9"), "^9", "a range is untouched");
    }

    /// A fake installed PM at `<store>/pm/<pm>/<version>/package/` with a real
    /// manifest + bin file, plus a `.npmrc` pointing the registry at an unroutable
    /// port — so a test that reaches the network fails fast instead of touching
    /// the real registry. `tag` keeps each test's store distinct: tests share a
    /// process (same pid) and run in parallel, so a version-only name would race.
    fn offline_store_with(tag: &str, version: &str) -> PathBuf {
        let store =
            std::env::temp_dir().join(format!("nub-pm-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let pkg = store.join("pm").join("pnpm").join(version).join("package");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "pnpm", "bin": { "pnpm": "bin/pnpm.cjs", "pnpx": "bin/pnpx.cjs" } }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("bin/pnpm.cjs"), "// fake pnpm launcher\n").unwrap();
        std::fs::write(store.join(".npmrc"), "registry=http://127.0.0.1:1/\n").unwrap();
        store
    }

    /// The ambient env can carry `npm_config_registry`, which outranks the test
    /// `.npmrc` and would re-route the must-not-hit-the-network assertions to a
    /// real registry. Process-global env is flaky to mutate under the parallel
    /// harness (same posture as `registry_base`'s test), so those tests skip.
    fn ambient_registry_override() -> bool {
        std::env::var("npm_config_registry").is_ok_and(|v| !v.trim().is_empty())
    }

    #[test]
    fn exact_cached_pin_provisions_offline() {
        // An exact pin with a cached install never consults the registry — the
        // dead-registry `.npmrc` proves it: any fetch would error, not succeed.
        let store = offline_store_with("exact", "9.5.0");
        let pin = PmPin {
            pm: Pm::Pnpm,
            version: Some("9.5.0+sha512.abc".to_string()), // hash suffix stripped first
        };
        let prov = provision_pm(&pin, &store).expect("offline cache hit");
        assert_eq!(prov.version, "9.5.0");
        assert!(prov.bin.ends_with("9.5.0/package/bin/pnpm.cjs"));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn range_pin_falls_back_to_best_cached_match_when_registry_is_down() {
        if ambient_registry_override() {
            return; // env registry outranks the dead-registry .npmrc — see helper
        }
        let store = offline_store_with("range", "9.5.0");
        // A second, lower cached version: the fallback must pick the highest match.
        let pkg = store.join("pm").join("pnpm").join("9.1.0").join("package");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "pnpm", "bin": { "pnpm": "bin/pnpm.cjs" } }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("bin/pnpm.cjs"), "// fake\n").unwrap();

        let pin = PmPin {
            pm: Pm::Pnpm,
            version: Some("^9".to_string()),
        };
        let prov = provision_pm(&pin, &store).expect("offline range fallback");
        assert_eq!(
            prov.version, "9.5.0",
            "highest cached satisfying version wins"
        );

        // An EXACT-but-uncached pin must NOT range-match the cache (an exact spec
        // parses as a caret VersionReq — 10.0.0 would falsely match a cached
        // 10.5.0): it surfaces the fetch error instead.
        let exact_miss = PmPin {
            pm: Pm::Pnpm,
            version: Some("9.4.0".to_string()),
        };
        assert!(
            provision_pm(&exact_miss, &store).is_err(),
            "uncached exact pin must not be satisfied by a cached sibling version"
        );

        // A dist-tag has no offline answer: fetch error, never a cache guess.
        let tag = PmPin {
            pm: Pm::Pnpm,
            version: Some("latest".to_string()),
        };
        assert!(provision_pm(&tag, &store).is_err());
        let _ = std::fs::remove_dir_all(&store);
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
        let node = crate::node::discovery::discover_node(&store).expect("a node to run pnpm under");
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
