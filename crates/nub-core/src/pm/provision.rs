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

use anyhow::{Context, Result, bail};

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
///   1. split any Corepack `+<algo>.<hex>` pin hash off the pinned version (the
///      suffix gates step 6 on the download path — see [`split_hash_suffix`]),
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
///   6. verify integrity BEFORE extraction (executables landing on disk): the
///      pin's embedded hash when present (the registry-independent trust
///      anchor), then the registry's dist integrity,
///   7. extract the `.tgz` and atomically `rename` into place,
///   8. uv-style `Installing…` / `✓ Installed…` on STDERR.
///
/// `pin.version` must be present — a [`PmPin`] with no version can't be
/// provisioned (the caller resolves the spec from a lockfile / `packageManager`
/// before reaching here). The returned bin is `<version>/package/<bin_subpath>`.
pub fn provision_pm(pin: &PmPin, store_root: &Path, project_root: &Path) -> Result<ProvisionedPm> {
    let pm = pin.pm;
    let raw = pin
        .version
        .as_deref()
        .with_context(|| format!("no version to provision for {pm}"))?;
    let (spec, pin_hash) = split_hash_suffix(raw);

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

    // Registry config from the PROJECT dir — a committed .npmrc (registry= /
    // //host/:_authToken=) must govern where the PM is downloaded from and how.
    // (It was read from the cache-store root before — a dir no project commits
    // anything into — so project mirrors/auth were silently ignored.)
    let cfg = registry::registry_config(project_root);
    let dist = match registry::resolve_version_authed(&cfg, &pm.to_string(), spec) {
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

    install(
        pm,
        &dist,
        &pm_store,
        &final_dir,
        pin_hash,
        cfg.auth.as_ref(),
    )?;
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
/// indirection). `pin_hash` is the pin's `<algo>.<hex>` suffix, when the pin
/// carried one — verified against the downloaded tarball before extraction.
fn install(
    pm: super::Pm,
    dist: &VersionDist,
    pm_store: &Path,
    final_dir: &Path,
    pin_hash: Option<&str>,
    auth: Option<&download::Auth>,
) -> Result<()> {
    // Sibling temp dir on the same filesystem → final placement is an atomic
    // rename. The guard cleans it up on every exit path. A failure here is the
    // canonical read-only-store symptom (a CI/container with an unwritable cache):
    // name the dir and the fix so it isn't an opaque ENOENT/EACCES.
    let work = pm_store.join(format!(".tmp-{}-{}", dist.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| {
        format!(
            "cannot create the package-manager store dir {} — its parent is missing or \
             read-only (set XDG_CACHE_HOME to a writable path)",
            work.display()
        )
    })?;
    let _guard = WorkGuard(work.clone());

    let started = Instant::now();
    let tarball = work.join("package.tgz");
    // uv/cargo-style progress: the announce line appears BEFORE the download (so
    // a slow fetch isn't silence), and on a TTY the ✓ line OVERWRITES it — a
    // finished session shows one line, not a redundant Installing/Installed
    // pair. Non-TTY (CI logs, pipes) keeps both lines: there's no cursor to
    // rewrite, and the announce timestamp is useful in a log.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut announced = false;
    download::download_to_file_auth(&dist.tarball, &tarball, auth, |_done, total| {
        if !announced {
            announced = true;
            let size = match total {
                Some(t) => format!(" ({} MB)", t / 1_000_000),
                None => String::new(),
            };
            if tty {
                eprint!("Installing {pm} {}{size}...", dist.version);
            } else {
                eprintln!("Installing {pm} {}{size}...", dist.version);
            }
        }
    })
    .with_context(|| format!("downloading {pm} {}", dist.version))?;

    // Verify BEFORE extracting. The pin's embedded hash comes first: it is the
    // registry-INDEPENDENT trust anchor (`nub pm pin` computed it from a tarball
    // it verified), so a tampered artifact fails against the committed digest
    // even if the registry's own metadata is complicit. Note this gates the
    // DOWNLOAD path only — an exact pin already in the store returned from the
    // cache scan before any download (a hit is trusted; the version-addressed
    // store posture in the module doc).
    if let Some(suffix) = pin_hash {
        verify_pin_hash(&tarball, suffix).with_context(|| {
            format!(
                "verifying {pm} {} against the packageManager pin hash",
                dist.version
            )
        })?;
    }
    registry::verify_integrity(&tarball, &dist.integrity)
        .with_context(|| format!("verifying {pm} {}", dist.version))?;

    // Extract into a clean sibling, then normalize its single top dir to
    // `package/`; renaming `staging` into place makes the install root
    // `<final_dir>/package/…` (the bin path callers expect) regardless of the
    // publisher's tarball root.
    let staging = work.join("staging");
    let top = extract_tgz(&tarball, &staging)?;
    normalize_top_dir(&staging, &top)?;

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

    // \r + clear-to-EOL rewrites the announce line on a TTY (it was printed
    // without a newline there); non-TTY just gets the second line.
    let rewrite = if tty { "\r\x1b[K" } else { "" };
    eprintln!(
        "{rewrite}✓ Installed {pm} {} in {:.1}s",
        dist.version,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Rename an extracted tarball's single top-level dir to `package/` when the
/// publisher used another name: npm/pnpm publish under `package/`, but **yarn
/// classic's tarball root is `yarn-v<version>/`**. The store's uniform
/// `<version>/package/<bin>` layout — what [`cached_bin`], the cache-hit checks,
/// and the returned bin path all assume — depends on this normalization; without
/// it a yarn install lands unrunnable and poisons its store dir.
fn normalize_top_dir(staging: &Path, top: &Path) -> Result<()> {
    let pkg = staging.join("package");
    if top != pkg {
        std::fs::rename(top, &pkg)
            .with_context(|| format!("normalizing {} to package/", top.display()))?;
    }
    Ok(())
}

/// Split a `packageManager`-style version into the bare spec and the optional
/// Corepack `+<algo>.<hex>` hash suffix: `10.0.0+sha512.abc…` →
/// `("10.0.0", Some("sha512.abc…"))`. The bare spec is what reaches the cache
/// scan and the registry; the suffix is the PIN HASH — the registry-independent
/// trust anchor `nub pm pin` writes from the artifact it verified — and it gates
/// the download path in [`install`] (see [`verify_pin_hash`]). nub does not
/// honor `COREPACK_INTEGRITY_KEYS` (signature keys are out of scope; the pin
/// hash plus the registry's `dist` integrity are the whole integrity story).
fn split_hash_suffix(version: &str) -> (&str, Option<&str>) {
    match version.split_once('+') {
        Some((v, suffix)) => (v, Some(suffix)),
        None => (version, None),
    }
}

/// Verify a downloaded tarball against the pin's `<algo>.<hex>` suffix. The
/// digest is HEX-encoded (corepack's format — `createHash(algo).digest("hex")`),
/// NOT the registry's base64 SRI. `sha512` (what corepack and `nub pm pin` write
/// today) and `sha224` (corepack's older default) are supported; anything else
/// is a fail-closed unsupported-algorithm error — a pin that *claims* a hash nub
/// can't check must never install silently unverified.
fn verify_pin_hash(file: &Path, suffix: &str) -> Result<()> {
    use sha2::{Digest, Sha224, Sha512};

    let (algo, want) = suffix.split_once('.').with_context(|| {
        format!("malformed pin hash suffix \"+{suffix}\" — expected +<algo>.<hex>")
    })?;
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let got = match algo {
        "sha512" => super::hex_lower(&Sha512::digest(&bytes)),
        "sha224" => super::hex_lower(&Sha224::digest(&bytes)),
        other => bail!(
            "unsupported pin hash algorithm \"{other}\" in \"+{suffix}\" — nub verifies \
             sha512 and sha224 (hex digests, corepack's format); refusing to install unverified"
        ),
    };
    if !got.eq_ignore_ascii_case(want) {
        bail!(
            "pin hash mismatch for {}: the packageManager pin expects {algo}.{want}, \
             the downloaded tarball is {algo}.{got}",
            file.display()
        );
    }
    Ok(())
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
    fn splits_the_corepack_hash_suffix_into_spec_and_pin_hash() {
        assert_eq!(
            split_hash_suffix("10.0.0+sha512.abc123"),
            ("10.0.0", Some("sha512.abc123"))
        );
        assert_eq!(
            split_hash_suffix("10.0.0"),
            ("10.0.0", None),
            "a hashless pin carries no claim to verify — provisioning is unaffected"
        );
        assert_eq!(
            split_hash_suffix("^9"),
            ("^9", None),
            "a range is untouched"
        );
    }

    #[test]
    fn pin_hash_verification_is_fail_closed_over_hex_digests() {
        let dir = std::env::temp_dir().join(format!("nub-pm-pinhash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob.tgz");
        std::fs::write(&f, b"abc").unwrap();

        // Known digests of "abc" — HEX (corepack's format), not base64 SRI.
        const SHA512_ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
        const SHA224_ABC: &str = "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7";

        verify_pin_hash(&f, &format!("sha512.{SHA512_ABC}")).expect("matching sha512 verifies");
        verify_pin_hash(&f, &format!("sha224.{}", SHA224_ABC.to_uppercase()))
            .expect("sha224 is accepted, case-insensitively");

        // A mismatch must fail naming BOTH digests, so a CI failure is
        // self-debugging without a rerun.
        let err = verify_pin_hash(&f, "sha512.deadbeef")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sha512.deadbeef"),
            "mismatch names the pinned digest: {err}"
        );
        assert!(
            err.contains(SHA512_ABC),
            "mismatch names the actual digest: {err}"
        );

        // An algorithm nub can't check is an error, never a silent skip.
        let err = verify_pin_hash(&f, &format!("sha1.{SHA224_ABC}"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unsupported") && err.contains("sha1"),
            "unknown algorithm fails closed naming it: {err}"
        );

        // A suffix with no `<algo>.` separator is malformed — also fail closed.
        assert!(verify_pin_hash(&f, "sha512").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_package_top_dir_is_normalized_to_the_store_layout() {
        // yarn classic's tarball root is `yarn-v<version>/`, not npm's `package/`
        // — the normalizer renames it so `<version>/package/<bin>` holds for
        // every PM (found live: an unnormalized yarn install left a store dir
        // with no `package/`, unrunnable and blocking every later install).
        let staging = std::env::temp_dir().join(format!("nub-pm-topdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let top = staging.join("yarn-v1.22.19");
        std::fs::create_dir_all(top.join("bin")).unwrap();
        std::fs::write(top.join("bin/yarn.js"), "// yarn\n").unwrap();

        normalize_top_dir(&staging, &top).unwrap();
        assert!(
            staging.join("package/bin/yarn.js").is_file(),
            "a foreign top dir must be renamed to package/ with its contents intact"
        );
        assert!(
            !top.exists(),
            "the original top dir is renamed away, not copied"
        );

        // An already-`package/` top dir (npm/pnpm tarballs) is a no-op.
        normalize_top_dir(&staging, &staging.join("package")).unwrap();
        assert!(staging.join("package/bin/yarn.js").is_file());
        let _ = std::fs::remove_dir_all(&staging);
    }

    /// A fake installed PM at `<store>/pm/<pm>/<version>/package/` with a real
    /// manifest + bin file, plus a `.npmrc` pointing the registry at an unroutable
    /// port — so a test that reaches the network fails fast instead of touching
    /// the real registry. `tag` keeps each test's store distinct: tests share a
    /// process (same pid) and run in parallel, so a version-only name would race.
    fn offline_store_with(tag: &str, version: &str) -> PathBuf {
        let store = std::env::temp_dir().join(format!("nub-pm-cache-{tag}-{}", std::process::id()));
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
        // The pin hash rides along but is NOT re-checked on a cache hit (a hit is
        // trusted — the hash gates the download path only).
        let store = offline_store_with("exact", "9.5.0");
        let pin = PmPin {
            pm: Pm::Pnpm,
            version: Some("9.5.0+sha512.abc".to_string()),
        };
        let prov = provision_pm(&pin, &store, &store).expect("offline cache hit");
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
        let prov = provision_pm(&pin, &store, &store).expect("offline range fallback");
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
            provision_pm(&exact_miss, &store, &store).is_err(),
            "uncached exact pin must not be satisfied by a cached sibling version"
        );

        // A dist-tag has no offline answer: fetch error, never a cache guess.
        let tag = PmPin {
            pm: Pm::Pnpm,
            version: Some("latest".to_string()),
        };
        assert!(provision_pm(&tag, &store, &store).is_err());
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

        let prov = provision_pm(&pin, &store, &store).expect("provision pnpm");
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
        let again = provision_pm(&pin, &store, &store).expect("cache hit");
        assert_eq!(again, prov);

        // Wiring check for the pin hash on the real download path: a FRESH store
        // (no cache to satisfy the pin) + a wrong claimed digest must fail closed
        // before anything lands in the store. The match path is the same code
        // minus the bail (unit-covered above), so only the mismatch is exercised
        // against the network.
        let fresh = std::env::temp_dir().join(format!("nub-pm-prov-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        let bad = PmPin {
            pm: Pm::Pnpm,
            version: Some(format!("10.0.0+sha512.{}", "0".repeat(128))),
        };
        let err = format!("{:#}", provision_pm(&bad, &fresh, &fresh).unwrap_err());
        assert!(
            err.contains("pin hash mismatch"),
            "a wrong pin hash must fail the download path closed, got: {err}"
        );
        assert!(
            !fresh.join("pm").join("pnpm").join("10.0.0").exists(),
            "a failed verification must not leave an install behind"
        );

        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&store);
    }
}
