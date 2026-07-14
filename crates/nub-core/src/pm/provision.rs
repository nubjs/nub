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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Join a registry-resolved version onto the PM store dir, asserting the result
/// stays UNDER the store. The STRUCTURAL half of the F0c containment, paired with
/// [`registry::validate_version`]'s char-blocklist: an ABSOLUTE or DRIVE-prefixed
/// version (`C:foo` on Windows) makes `Path::join` DISCARD the base and escape the
/// store, so the bin nub then writes + EXECUTES would land outside `<store>/pm/<pm>/`.
/// This is the exact escape class a char-blocklist is most apt to miss a
/// metacharacter for (the Windows `:` slipped through once). `validate_version`
/// already rejects those at resolution, so in practice this never fires — it is
/// belt-and-suspenders. The two layers are complementary: this catches the
/// base-discarding join (absolute/drive), while the separator/`..` class stays with
/// `validate_version` (`PathBuf::starts_with` is a lexical component-prefix and does
/// NOT normalize `..`, so it is not a traversal guard on its own — but it is
/// component-wise, so a sibling-prefix dir like `…/pm-evil` does not spuriously pass).
fn store_version_dir(pm_store: &Path, version: &str) -> Result<PathBuf> {
    let dir = pm_store.join(version);
    if !dir.starts_with(pm_store) {
        bail!(
            "refusing to use {version:?} as a store path component — it escapes {}",
            pm_store.display()
        );
    }
    Ok(dir)
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
///   8. `Using…` / `Installing…` / `Installed…` on STDERR (see [`install`]).
///
/// `pin.version` must be present — a [`PmPin`] with no version can't be
/// provisioned (the caller resolves the spec from a lockfile / `packageManager`
/// before reaching here). `resolved_from` is preformatted pin provenance (e.g.
/// `packageManager`) appended to the `Installing…` announce, mirroring
/// `provision_node`; `None` where the surrounding output already explains the
/// install (`nub pm use`, the shim's own announce). The returned bin is
/// `<version>/package/<bin_subpath>`.
pub fn provision_pm(
    pin: &PmPin,
    store_root: &Path,
    project_root: &Path,
    resolved_from: Option<&str>,
) -> Result<ProvisionedPm> {
    provision_pm_announced(pin, store_root, project_root, resolved_from, None)
}

/// [`provision_pm`] with an optional `on_resolved` hook fired exactly once with the
/// concrete version the moment it is known — BEFORE any cache-hit early return and
/// BEFORE the `Installing…`/`Installed…` progress, so a caller (the PM shim) can
/// print its own header (`pnpm@9.5.0 (via nub shim)`) ahead of the install readout.
/// Passing `on_resolved` ALSO suppresses provisioning's own `Using <pm> <version>`
/// line: the shim's header conveys the version, so the `Using…` line is redundant
/// there. Direct `nub install`/`nub run` provisioning passes `None` and keeps the
/// `Using…` line unchanged.
pub fn provision_pm_announced(
    pin: &PmPin,
    store_root: &Path,
    project_root: &Path,
    resolved_from: Option<&str>,
    on_resolved: Option<&dyn Fn(&str)>,
) -> Result<ProvisionedPm> {
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
            // Warm cache: the concrete version is `spec` itself. Fire the hook so
            // the shim header still prints, then return silently (no `Installing…`).
            if let Some(cb) = on_resolved {
                cb(spec);
            }
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
                    if let Some(cb) = on_resolved {
                        cb(&version);
                    }
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

    // The concrete version is now known (a range/dist-tag resolved). Fire the hook
    // before the cache check / install so the shim header precedes any progress.
    if let Some(cb) = on_resolved {
        cb(&dist.version);
    }

    let final_dir = store_version_dir(&pm_store, &dist.version)?;
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
        // Host-match the registry auth to the tarball before attaching it: a
        // packument's `dist.tarball` can name a foreign host, and the bearer
        // token must never leave the registry's own origin (N1b). The packument
        // fetch above already used the full `cfg.auth`.
        registry::auth_for_tarball(&cfg, &dist.tarball),
        resolved_from,
        on_resolved.is_some(),
    )?;
    Ok(ProvisionedPm {
        bin,
        version: dist.version,
    })
}

/// Provision an EXACT, already-resolved version into the store from a tarball the
/// caller has **already downloaded and verified** — the single-download path for
/// `nub pm use` / `nub pm update` (the pin flow that fetched the tarball once to
/// compute the `+sha512.<hex>` pin hash, and feeds those same bytes in here rather
/// than re-downloading them; the double-download this kills was the cold-cache bug,
/// 2026-06-11).
///
/// Same cache posture and store layout as [`provision_pm`], minus the network: the
/// exact version cache-checks `<store_root>/pm/<pm>/<version>/` first and returns a
/// silent hit (a warm `use` re-pin stays zero-cost — the lockfile/declaration
/// writes the caller does afterward are unchanged); a miss extracts+places the
/// supplied `tarball` (already integrity-verified by the caller, so this path does
/// NOT re-verify — see the contract below) into the version-addressed store.
///
/// CONTRACT — the caller MUST, before calling:
///   1. resolve `dist` against the same registry config provisioning would use
///      (`registry_config(project_root)` — the authed/mirror path), and
///   2. download `tarball` from `dist.tarball` and verify it against
///      `dist.integrity` (and, where a pin hash exists, it was computed from these
///      same verified bytes). This function trusts the file; it re-verifies
///      nothing. It exists ONLY to avoid a second download of bytes the caller
///      already has and already verified — every other caller must keep using the
///      network [`provision_pm`] entry, which downloads + verifies itself.
///
/// No `Using…/Installing…/Installed…` block is printed: the caller (`nub pm use`)
/// already announced the fetch with its own `Fetching <pm> <version> (N MB)…` line,
/// so a second announce here would duplicate it. Output stays the caller's.
pub fn provision_pm_from_tarball(
    pm: super::Pm,
    dist: &VersionDist,
    tarball: &Path,
    store_root: &Path,
) -> Result<ProvisionedPm> {
    let pm_store = store_root.join("pm").join(pm.to_string());
    let final_dir = store_version_dir(&pm_store, &dist.version)?;
    let bin = final_dir.join("package").join(&dist.bin_subpath);

    // Cache hit on the exact resolved version → done, zero work (the warm re-pin
    // case: `use` on a version already in the store extracts nothing).
    if bin.is_file() {
        return Ok(ProvisionedPm {
            bin,
            version: dist.version.clone(),
        });
    }

    place_verified_tarball(pm, dist, &pm_store, &final_dir, tarball)?;
    Ok(ProvisionedPm {
        bin,
        version: dist.version.clone(),
    })
}

/// The runnable bin of an already-installed `<pm_store>/<version>/`, or `None`
/// when the version isn't cached (or its install is unreadable/incomplete —
/// callers then take the network path, whose installer repairs only that
/// incomplete version entry from verified staging).
fn cached_bin(pm_store: &Path, version: &str) -> Option<PathBuf> {
    let pkg_dir = pm_store.join(version).join("package");
    let raw = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(crate::strip_utf8_bom(&raw)).ok()?;
    let bin = pkg_dir.join(registry::bin_subpath(&manifest)?);
    bin.is_file().then_some(bin)
}

/// True when `version` of `pm` is already extracted and runnable in the store —
/// the exact predicate [`provision_pm_from_tarball`]'s cache-hit arm trusts
/// (`cached_bin` over the same `<store_root>/pm/<pm>/<version>/package` path).
///
/// The warm-exact-re-pin seam: `nub pm use <pm>@<exact>` consults this to decide
/// whether it can skip the network entirely (the pin hash already lives in the
/// manifest; the bytes are only needed when the store is cold). A `false` here
/// means "not present or incomplete" — the caller must then fetch.
pub fn pm_version_cached(pm: super::Pm, version: &str, store_root: &Path) -> bool {
    let pm_store = store_root.join("pm").join(pm.to_string());
    cached_bin(&pm_store, version).is_some()
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
#[allow(clippy::too_many_arguments)] // distinct provisioning inputs; a struct would be pure ceremony
fn install(
    pm: super::Pm,
    dist: &VersionDist,
    pm_store: &Path,
    final_dir: &Path,
    pin_hash: Option<&str>,
    auth: Option<&download::Auth>,
    resolved_from: Option<&str>,
    quiet_using: bool,
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
    // Same three-line shape as `provision_node`: `Using <pm> <version>` (with
    // pin provenance when known) states what was resolved, the `Installing`
    // announce appears BEFORE the download (so a slow fetch isn't silence), and
    // on a TTY the `Installed` line OVERWRITES the announce — a finished session
    // shows two lines. Non-TTY (CI logs, pipes) keeps all three. `quiet_using`
    // drops the `Using…` line for the PM shim, whose own `<pm>@<version> (via nub
    // shim)` header (printed before this) already states the resolved version.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if !quiet_using {
        match resolved_from {
            Some(p) => eprintln!("Using {pm} {} (resolved from {p})", dist.version),
            None => eprintln!("Using {pm} {}", dist.version),
        }
    }
    let mut announced = false;
    download::download_to_file_auth(&dist.tarball, &tarball, auth, |_done, total| {
        if !announced {
            announced = true;
            let size = match total {
                Some(t) => format!(" ({} MB)", t / 1_000_000),
                None => String::new(),
            };
            if tty {
                eprint!("Installing...{size}");
            } else {
                eprintln!("Installing...{size}");
            }
        }
    })
    .with_context(|| format!("downloading {pm} {}", dist.version))?;

    // Verify BEFORE extracting. The pin's embedded hash comes first: it is the
    // registry-INDEPENDENT trust anchor (`nub pm use` computed it from a tarball
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

    extract_and_place(pm, dist, pm_store, final_dir, &tarball)?;

    // \r + clear-to-EOL rewrites the Installing line on a TTY (it was printed
    // without a newline there); non-TTY just gets a third line.
    let rewrite = if tty { "\r\x1b[K" } else { "" };
    eprintln!(
        "{rewrite}Installed in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Extract a verified `.tgz` into the store and atomically place it — the tail
/// shared by the networked [`install`] (which downloads + verifies first) and the
/// pre-downloaded [`provision_pm_from_tarball`] path (whose caller already
/// downloaded + verified). VERIFICATION IS THE CALLER'S JOB: this runs no integrity
/// check, so every path into it must have verified the tarball against
/// `dist.integrity` (and any pin hash) first. The extraction normalizes the
/// publisher's top dir to `package/` and the rename keeps a concurrent install's
/// result if one beat us.
fn extract_and_place(
    pm: super::Pm,
    dist: &VersionDist,
    pm_store: &Path,
    final_dir: &Path,
    tarball: &Path,
) -> Result<()> {
    // A sibling work dir on the same filesystem → the place is an atomic rename.
    // (The networked `install` already has one for the download; this stands up
    // its own so the pre-downloaded path is self-contained.) Guard cleans up.
    let work = pm_store.join(format!(
        ".tmp-place-{}-{}",
        dist.version,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| {
        format!(
            "cannot create the package-manager store dir {} — its parent is missing or \
             read-only (set XDG_CACHE_HOME to a writable path)",
            work.display()
        )
    })?;
    let _guard = WorkGuard(work.clone());

    // Extract into a clean sibling, then normalize its single top dir to
    // `package/`; renaming `staging` into place makes the install root
    // `<final_dir>/package/…` (the bin path callers expect) regardless of the
    // publisher's tarball root.
    let staging = work.join("staging");
    let top = extract_tgz(tarball, &staging)?;
    normalize_top_dir(&staging, &top)?;

    let staging_bin = staging.join("package").join(&dist.bin_subpath);
    let final_bin = final_dir.join("package").join(&dist.bin_subpath);
    if !staging_bin.is_file() {
        bail!(
            "cannot install {pm} {}: the verified package is missing the expected launcher {}",
            dist.version,
            final_bin.display()
        );
    }

    if final_bin.is_file() {
        return Ok(());
    }
    if final_dir.exists() {
        return recover_incomplete_version(pm, dist, pm_store, final_dir, &staging);
    }

    #[cfg(test)]
    run_before_initial_rename_test_hook(final_dir);

    match std::fs::rename(&staging, final_dir) {
        Ok(()) => require_launcher(pm, dist, &final_bin),
        Err(_) if final_bin.is_file() => Ok(()),
        Err(_) if final_dir.exists() => {
            recover_incomplete_version(pm, dist, pm_store, final_dir, &staging)
        }
        Err(e) => Err(e).with_context(|| placement_context(pm, dist, final_dir)),
    }
}

fn recover_incomplete_version(
    pm: super::Pm,
    dist: &VersionDist,
    pm_store: &Path,
    final_dir: &Path,
    staging: &Path,
) -> Result<()> {
    let final_bin = final_dir.join("package").join(&dist.bin_subpath);
    if final_bin.is_file() {
        return Ok(());
    }

    let quarantine = match move_to_unique_quarantine(pm_store, final_dir, &dist.version) {
        Ok(path) => path,
        Err(_) if final_bin.is_file() => return Ok(()),
        Err(e) => return Err(e),
    };
    let _quarantine_guard = WorkGuard(quarantine.clone());
    match std::fs::rename(staging, final_dir) {
        Ok(()) => require_launcher(pm, dist, &final_bin),
        Err(_) if final_bin.is_file() => Ok(()),
        Err(e) => Err(e).with_context(|| placement_context(pm, dist, final_dir)),
    }
}

fn move_to_unique_quarantine(pm_store: &Path, final_dir: &Path, version: &str) -> Result<PathBuf> {
    rename_to_unique_sibling(final_dir, || next_quarantine_path(pm_store, version)).with_context(
        || {
            format!(
                "cannot quarantine incomplete package-manager cache entry {}",
                final_dir.display()
            )
        },
    )
}

fn require_launcher(pm: super::Pm, dist: &VersionDist, launcher: &Path) -> Result<()> {
    if !launcher.is_file() {
        bail!(
            "installing {pm} {} did not produce the expected launcher {}",
            dist.version,
            launcher.display()
        );
    }
    Ok(())
}

fn placement_context(pm: super::Pm, dist: &VersionDist, final_dir: &Path) -> String {
    format!(
        "installing {pm} {} into {}",
        dist.version,
        final_dir.display()
    )
}

fn rename_to_unique_sibling(
    source: &Path,
    mut next: impl FnMut() -> PathBuf,
) -> std::io::Result<PathBuf> {
    loop {
        let candidate = next();
        match std::fs::rename(source, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(_)
                if std::fs::symlink_metadata(source).is_ok()
                    && std::fs::symlink_metadata(&candidate).is_ok() =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

fn next_quarantine_path(pm_store: &Path, version: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    pm_store.join(format!(
        ".tmp-replaced-{version}-{}-{sequence}",
        std::process::id()
    ))
}

#[cfg(test)]
type BeforeInitialRenameHook = Box<dyn Fn(&Path) + Send>;

#[cfg(test)]
static BEFORE_INITIAL_RENAME_HOOK: std::sync::Mutex<Option<BeforeInitialRenameHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn run_before_initial_rename_test_hook(final_dir: &Path) {
    if let Some(hook) = BEFORE_INITIAL_RENAME_HOOK.lock().unwrap().as_ref() {
        hook(final_dir);
    }
}

/// The silent (no announce, no `Installed` line) extract+place for the
/// pre-downloaded pin path — a thin name over [`extract_and_place`] so
/// [`provision_pm_from_tarball`] reads at the same level as `install`.
fn place_verified_tarball(
    pm: super::Pm,
    dist: &VersionDist,
    pm_store: &Path,
    final_dir: &Path,
    tarball: &Path,
) -> Result<()> {
    extract_and_place(pm, dist, pm_store, final_dir, tarball)
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
/// trust anchor `nub pm use` writes from the artifact it verified — and it gates
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
/// NOT the registry's base64 SRI. `sha512` (what corepack and `nub pm use` write
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

    /// F0c structural backstop: a legit version stays under the store; an absolute
    /// component (the cross-platform stand-in for the Windows `C:foo` drive-relative
    /// escape, where `Path::join` discards the base) is refused. `validate_version`
    /// blocks both before they reach here — this proves the second line of defense.
    #[test]
    fn store_version_dir_contains_legit_and_rejects_a_base_discarding_join() {
        let store = Path::new("/var/cache/nub/pm/pnpm");
        assert_eq!(
            store_version_dir(store, "11.9.0").unwrap(),
            store.join("11.9.0")
        );
        assert!(
            store_version_dir(store, "/etc/evil").is_err(),
            "an absolute version makes join discard the base — must be refused"
        );
    }
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
        let prov = provision_pm(&pin, &store, &store, None).expect("offline cache hit");
        assert_eq!(prov.version, "9.5.0");
        assert!(prov.bin.ends_with("9.5.0/package/bin/pnpm.cjs"));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn on_resolved_hook_fires_with_the_concrete_version_on_a_warm_cache() {
        // The shim header (`pnpm@9.5.0 (via nub shim)`) is driven by this hook: it
        // must fire even on a warm cache hit (which returns before any install
        // progress) so the notice still prints, and it must carry the concrete
        // resolved version (the exact pin's own version here).
        let store = offline_store_with("hook-warm", "9.5.0");
        let pin = PmPin {
            pm: Pm::Pnpm,
            version: Some("9.5.0+sha512.abc".to_string()),
        };
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let prov = provision_pm_announced(
            &pin,
            &store,
            &store,
            None,
            Some(&|v: &str| {
                seen.borrow_mut().push(v.to_string());
            }),
        )
        .expect("offline cache hit");
        assert_eq!(prov.version, "9.5.0");
        assert_eq!(
            *seen.borrow(),
            vec!["9.5.0".to_string()],
            "the hook fires exactly once with the concrete version"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn on_resolved_hook_fires_with_the_concrete_version_on_offline_range_fallback() {
        if ambient_registry_override() {
            return; // env registry outranks the dead-registry .npmrc — see helper
        }
        // The offline range-fallback path (registry unreachable + range pin + cached
        // version present) must fire `on_resolved` with the resolved concrete version
        // so the PM-shim header (`pnpm@9.5.0 (via nub shim)`) still prints even when
        // the registry is down and the version was inferred from the cache.
        let store = offline_store_with("hook-offline-range", "9.5.0");
        let pin = PmPin {
            pm: Pm::Pnpm,
            version: Some("^9".to_string()),
        };
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let prov = provision_pm_announced(
            &pin,
            &store,
            &store,
            None,
            Some(&|v: &str| {
                seen.borrow_mut().push(v.to_string());
            }),
        )
        .expect("offline range fallback with on_resolved hook");
        assert_eq!(prov.version, "9.5.0");
        assert_eq!(
            *seen.borrow(),
            vec!["9.5.0".to_string()],
            "the hook fires exactly once with the concrete cached version"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    const FAKE_PNPM_LAUNCHER: &[u8] = b"// fake pnpm launcher\n";

    /// Author a `package/`-rooted pnpm-shaped `.tgz` (gzip + tar) on disk — what
    /// the registry serves and what `nub pm use` downloads once, then hands to
    /// [`provision_pm_from_tarball`]. Same shape `extract.rs` authors in its own
    /// tests; restated here (three lines) rather than crossing the module seam.
    fn write_pnpm_tgz(archive: &Path) {
        write_pnpm_tgz_with_launcher(archive, Some(FAKE_PNPM_LAUNCHER));
    }

    fn write_pnpm_tgz_with_launcher(archive: &Path, launcher: Option<&[u8]>) {
        use flate2::{Compression, write::GzEncoder};
        let manifest = br#"{ "name": "pnpm", "bin": { "pnpm": "bin/pnpm.cjs" } }"#;
        let gz = GzEncoder::new(std::fs::File::create(archive).unwrap(), Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (path, body) in [("package/package.json", manifest.as_slice())]
            .into_iter()
            .chain(launcher.map(|body| ("package/bin/pnpm.cjs", body)))
        {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            tar.append_data(&mut h, path, body).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    fn fake_pnpm_dist() -> VersionDist {
        VersionDist {
            version: "9.12.0".to_string(),
            tarball: "http://127.0.0.1:1/never-fetched.tgz".to_string(),
            integrity: registry::Integrity::Sha512("unused-caller-already-verified".to_string()),
            bin_subpath: PathBuf::from("bin/pnpm.cjs"),
        }
    }

    fn assert_no_placement_debris(pm_store: &Path) {
        let leftovers = std::fs::read_dir(pm_store)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with(".tmp-place-") || name.starts_with(".tmp-replaced-"))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "placement work and quarantine dirs are cleaned: {leftovers:?}"
        );
    }

    #[test]
    fn provision_from_tarball_installs_without_a_second_download_and_warm_hits_silently() {
        // The single-download contract: `nub pm use`/`update` fetch + verify the
        // tarball ONCE (for the pin hash), then install into the store FROM THAT
        // FILE — never re-downloading. This is the pre-downloaded entry. The
        // dead-registry `.npmrc` in the store proves no network is consulted: any
        // fetch would error against 127.0.0.1:1, so a passing run means the supplied
        // tarball alone produced a runnable install.
        let store = offline_store_with("from-tarball", "0.0.0-unused"); // seeds a dead .npmrc
        let _ = std::fs::remove_dir_all(store.join("pm")); // drop the seed install; we install fresh

        let tgz = store.join("pnpm-9.12.0.tgz");
        write_pnpm_tgz(&tgz);
        // The bogus URL in this dist fails fast if the pre-downloaded path ever fetches.
        let dist = fake_pnpm_dist();

        // Cold: extracts the supplied tarball into the version-addressed store.
        let cold = provision_pm_from_tarball(Pm::Pnpm, &dist, &tgz, &store)
            .expect("install from the pre-downloaded tarball, no network");
        assert_eq!(cold.version, "9.12.0");
        assert!(
            cold.bin.ends_with("9.12.0/package/bin/pnpm.cjs") && cold.bin.is_file(),
            "the installed bin lands at the store layout and is runnable"
        );

        // Warm re-pin: the version is already in the store, so this is a silent
        // cache hit that extracts nothing — it doesn't even need the tarball. Delete
        // the file first to prove the warm path never touches it.
        std::fs::remove_file(&tgz).unwrap();
        let warm = provision_pm_from_tarball(Pm::Pnpm, &dist, &tgz, &store)
            .expect("warm cache hit needs neither network nor the tarball");
        assert_eq!(
            warm, cold,
            "the warm hit returns the identical bin + version"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn provision_from_tarball_repairs_an_incomplete_version_entry() {
        let store = offline_store_with("from-tarball-repair", "0.0.0-unused");
        let _ = std::fs::remove_dir_all(store.join("pm"));

        let tgz = store.join("pnpm-9.12.0.tgz");
        write_pnpm_tgz(&tgz);
        let dist = fake_pnpm_dist();
        let final_dir = store.join("pm/pnpm/9.12.0");
        std::fs::create_dir_all(final_dir.join("package")).unwrap();
        std::fs::write(final_dir.join("stale-sentinel"), "stale").unwrap();

        let provisioned = provision_pm_from_tarball(Pm::Pnpm, &dist, &tgz, &store)
            .expect("a verified replacement repairs an incomplete version entry");
        assert_eq!(
            std::fs::read(&provisioned.bin).unwrap(),
            FAKE_PNPM_LAUNCHER,
            "the replacement launcher lands at the returned path"
        );
        assert!(
            !final_dir.join("stale-sentinel").exists(),
            "recovery replaces the whole version entry rather than merging trees"
        );
        assert_no_placement_debris(&store.join("pm/pnpm"));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn placement_preserves_a_valid_concurrent_winner() {
        let store = offline_store_with("from-tarball-winner", "0.0.0-unused");
        let _ = std::fs::remove_dir_all(store.join("pm"));
        let pm_store = store.join("pm/pnpm");
        let final_dir = pm_store.join("9.12.0");
        let winner_bin = final_dir.join("package/bin/pnpm.cjs");
        assert!(!final_dir.exists());

        let tgz = store.join("pnpm-9.12.0.tgz");
        write_pnpm_tgz_with_launcher(&tgz, Some(b"candidate\n"));
        let hook_target = final_dir.clone();
        *BEFORE_INITIAL_RENAME_HOOK.lock().unwrap() = Some(Box::new(move |current| {
            if current == hook_target.as_path() {
                let winner = current.join("package/bin/pnpm.cjs");
                std::fs::create_dir_all(winner.parent().unwrap()).unwrap();
                std::fs::write(winner, b"winner\n").unwrap();
            }
        }));
        let provisioned = provision_pm_from_tarball(Pm::Pnpm, &fake_pnpm_dist(), &tgz, &store);
        *BEFORE_INITIAL_RENAME_HOOK.lock().unwrap() = None;
        let provisioned = provisioned.expect("a valid concurrent winner is adopted");

        assert_eq!(provisioned.bin, winner_bin);
        assert_eq!(std::fs::read(&winner_bin).unwrap(), b"winner\n");
        assert_no_placement_debris(&pm_store);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn quarantine_rename_skips_a_stale_name_collision() {
        let store = offline_store_with("quarantine-collision", "0.0.0-unused");
        let source = store.join("9.12.0");
        let stale_quarantine = store.join("stale-quarantine");
        let fresh_quarantine = store.join("fresh-quarantine");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("incumbent"), "incumbent").unwrap();
        std::fs::create_dir_all(&stale_quarantine).unwrap();
        std::fs::write(stale_quarantine.join("sentinel"), "stale").unwrap();
        let mut candidates = [stale_quarantine.clone(), fresh_quarantine.clone()].into_iter();

        let quarantine = rename_to_unique_sibling(&source, || candidates.next().unwrap()).unwrap();
        assert_eq!(quarantine, fresh_quarantine);
        assert!(fresh_quarantine.join("incumbent").is_file());
        assert!(stale_quarantine.join("sentinel").is_file());
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_malformed_replacement_does_not_disturb_an_incomplete_incumbent() {
        let store = offline_store_with("from-tarball-incomplete", "0.0.0-unused");
        let _ = std::fs::remove_dir_all(store.join("pm"));
        let pm_store = store.join("pm/pnpm");
        let final_dir = pm_store.join("9.12.0");
        let tgz = store.join("pnpm-9.12.0.tgz");
        write_pnpm_tgz_with_launcher(&tgz, None);
        let dist = fake_pnpm_dist();

        std::fs::create_dir_all(final_dir.join("package")).unwrap();
        std::fs::write(final_dir.join("stale-sentinel"), "stale").unwrap();
        let incumbent_err = provision_pm_from_tarball(Pm::Pnpm, &dist, &tgz, &store)
            .unwrap_err()
            .to_string();
        assert!(
            incumbent_err.contains("verified package is missing the expected launcher"),
            "unexpected error: {incumbent_err}"
        );
        assert_eq!(
            std::fs::read_to_string(final_dir.join("stale-sentinel")).unwrap(),
            "stale",
            "staging validation fails before moving an incomplete incumbent"
        );
        assert_no_placement_debris(&pm_store);
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
        let prov = provision_pm(&pin, &store, &store, None).expect("offline range fallback");
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
            provision_pm(&exact_miss, &store, &store, None).is_err(),
            "uncached exact pin must not be satisfied by a cached sibling version"
        );

        // A dist-tag has no offline answer: fetch error, never a cache guess.
        let tag = PmPin {
            pm: Pm::Pnpm,
            version: Some("latest".to_string()),
        };
        assert!(provision_pm(&tag, &store, &store, None).is_err());
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

        let prov = provision_pm(&pin, &store, &store, None).expect("provision pnpm");
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
        let again = provision_pm(&pin, &store, &store, None).expect("cache hit");
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
        let err = format!(
            "{:#}",
            provision_pm(&bad, &fresh, &fresh, None).unwrap_err()
        );
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
