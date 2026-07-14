//! Dynamic per-version phantom scan → disk-eject. Unconditionally ON for users
//! (maintainer decision 2026-07-06): there is no user-facing opt-out. The one
//! escape hatch for a suspected eject bug is disabling the global virtual store
//! entirely (full disk materialization via `node-linker`/`disableGlobalVirtualStore`),
//! which sidesteps the whole symlink+eject machinery. [`enabled`] carries the sole
//! remaining off-switch: an INTERNAL, undocumented `__NUB_*` test seam the phantom
//! test suite + framework/verify agents use to reproduce the pre-eject break as an
//! A/B control.
//!
//! This REPLACES the old hand-curated static disk-materialize list (capped at a
//! corpus, stale on new versions) by SCANNING each installed dependency
//! version's real published code: does it, along its reachable
//! `exports`/`main`/`bin` graph, statically and unguardedly import a package it
//! does not declare? A version's code is immutable, so the verdict is computed
//! once per content-fingerprint and cached machine-wide.
//!
//! Placement — EXTRACT TIME, not post-link. The scan is registered as an aube
//! store extract hook ([`aube_store::set_extract_hook`]) that fires at the end of
//! each tarball import, on the fetch/blocking fan-out thread, so per-version
//! analysis OVERLAPS the network-bound fetch phase (the scan CPU hides under
//! fetch's idle cores) instead of adding a serial post-link pass. Each verdict is
//! written to a per-content sidecar.
//!
//! The sidecars are CONSUMED by the disk-materialize expansion hook
//! ([`crate::pm_engine::phantom_closure`]): it reads them to seed the
//! selective-subtree closure with each flagged importer, so a poisoned version is
//! ejected project-local through #319's graph-aware materialization plan. This
//! module is the PRODUCER (scan + sidecar) half; `phantom_closure` is the
//! consumer half. The two share [`store_v1_dir`]/[`phantom_cache_dir`] so their
//! store handle and sidecar path derive from ONE base, and both build the sidecar
//! path through the single [`sidecar_path`] helper, so the fingerprint keying and
//! the scanner-version segment cannot drift apart. The sidecar path folds
//! [`PHANTOM_SCANNER_VERSION`] so a scanner-logic improvement re-scans already
//! cached content instead of serving the stale verdict its immutable bytes would
//! otherwise key forever.
//!
//! Under the internal A/B seam ([`enabled`] returns false) this module registers
//! nothing — no extract hook — so the install path is byte-identical to a build
//! without the scanner (a pure-symlink tree).

use std::path::{Path, PathBuf};

use aube_store::{PackageIndex, index_content_fingerprint};
use nub_phantom_scan::{ScanResult, scan_index};

/// Whether dynamic phantom detection + ancestor-closure eject is armed.
/// Unconditionally ON for users — there is NO user-facing opt-out (the removed
/// `NUB_DYNAMIC_PHANTOM_EJECT` user knob is dead and ignored). Off only under the
/// internal A/B seam below. This is the SINGLE arm both halves gate on — the
/// extract-time PRODUCER here and the link-time CONSUMER
/// ([`crate::pm_engine::phantom_closure`]) call this one function, and the
/// install-state fingerprint ([`settings_fingerprint`]) folds THIS value, so
/// detection, closure, and warm-tree invalidation can never drift.
pub(crate) fn enabled() -> bool {
    !eject_disabled(std::env::var(INTERNAL_EJECT_DISABLE_VAR).ok().as_deref())
}

/// INTERNAL, UNDOCUMENTED test seam — NOT a user knob. Truthy turns phantom-eject
/// OFF so the phantom test suite + framework/verify agents can reproduce the
/// pre-eject break as an A/B control against a real built binary (the `cfg(test)`
/// route can't, since those agents run `target/fast/nub`, not a test build). The
/// `__NUB_` double-underscore prefix marks internal plumbing — the brand boundary
/// exempts internal `__NUB_*` sentinels; this one is never documented and users
/// must not rely on it. Deliberately distinct from the removed public var so a
/// stale `NUB_DYNAMIC_PHANTOM_EJECT=0` in a user's env has zero effect.
const INTERNAL_EJECT_DISABLE_VAR: &str = "__NUB_PHANTOM_EJECT_DISABLE";

/// Pure predicate for the internal disable seam, split from the env read so its
/// truthiness contract is testable without mutating the process-global env. A
/// truthy value disables; unset / empty / any other value keeps eject ON.
fn eject_disabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// The effective phantom-eject setting as a stable token, folded into aube's
/// install-state `settings_hash` through the embedder `extra_settings_fingerprint`
/// hook (nub's [`crate::pm_engine::identity::NUB`] profile points that hook here).
/// The setting is nub's, not an aube setting, so it can't ride the resolved-settings
/// hash — this seam is what makes it invalidate the warm tree.
///
/// For users the token is CONSTANT-ON: the dead on/off toggle is gone, so it folds
/// only [`PHANTOM_SCANNER_VERSION`]. That fold is what makes a scanner-logic bump
/// COMPLETE rather than a half-fix — the bump re-scans content into a new sidecar
/// path, but on a warm tree with an unchanged lockfile aube would SKIP the link
/// phase and never apply the improved verdict; changing this token forces the link
/// to re-run so the consumer picks up the new-version sidecars.
///
/// The token still branches on [`enabled`] SOLELY for the internal A/B seam: when
/// an agent flips [`INTERNAL_EJECT_DISABLE_VAR`] the token changes, so a warm tree
/// re-links to the pure-symlink shape and the pre-eject break reproduces. Users
/// never reach that branch (the seam is undocumented internal plumbing).
pub(crate) fn settings_fingerprint() -> String {
    settings_token(enabled())
}

/// Pure token builder, split from [`settings_fingerprint`] so the fold contract is
/// testable without mutating the process-global `enabled()` env.
fn settings_token(enabled: bool) -> String {
    if enabled {
        format!("phantom_scanner={PHANTOM_SCANNER_VERSION}")
    } else {
        "phantom_eject=disabled".to_string()
    }
}

/// Register the extract-time scan hook with the embedded engine. Called once at
/// engine-session build. No-op only under the internal A/B seam ([`enabled`]
/// false), in which case the path pulls in nothing and stays byte-identical.
/// The registration is process-global set-once; a second call is ignored. The
/// link-time consumption of the sidecars this writes lives in
/// [`crate::pm_engine::phantom_closure`], not here.
pub fn register() {
    if !enabled() {
        return;
    }
    let Some(dir) = phantom_cache_dir() else {
        return;
    };
    // Extract-time scan: overlap per-version analysis with the fetch phase.
    aube_store::set_extract_hook(Box::new(move |index: &PackageIndex| {
        scan_and_cache(&dir, index);
    }));
}

/// Scan one freshly-imported package index and persist its verdict to the
/// per-content sidecar. Best-effort throughout — any failure simply leaves no
/// sidecar for the linker to find. The linker retries the scan from the CAS;
/// only an unavailable index or failed retry degrades to "no eject" (a scan
/// miss must never itself force materialization).
///
/// Panic-safety rests on the scan being panic-free BY CONSTRUCTION, not on the
/// `catch_unwind`: oxc reports an unparseable/hostile file via a return flag (not
/// an unwind), `serde`/`fs` return `Result`, and the graph walk is depth- and
/// size-bounded — so a crafted tarball degrades to a scan miss, never a crash.
/// The `catch_unwind` is a redundant guard that only engages under an unwinding
/// profile (dev/test); the shipped release profile is `panic = "abort"`, where it
/// is inert. Do not treat it as a production safety net.
///
/// The written JSON is the serialized [`nub_phantom_scan::ScanResult`], read back
/// by the CONSUMER ([`crate::pm_engine::phantom_closure`]) — which, being in
/// nub-cli, deserializes it into the typed `ScanResult` (no cross-fork string
/// coupling) to seed the disk-materialize closure.
fn scan_and_cache(dir: &Path, index: &PackageIndex) {
    let fingerprint = index_content_fingerprint(index);
    let sidecar = sidecar_path(dir, &fingerprint);
    // Cross-process / warm cache hit: this exact content was already scanned
    // UNDER THE CURRENT SCANNER VERSION (the version is in `sidecar`'s path, so a
    // scanner bump makes this `exists()` false and forces a re-scan). The verdict
    // is a pure function of the immutable bytes + scanner logic, so a concurrent
    // first-writer race is benign (identical result) — skip the redundant scan.
    if sidecar.exists() {
        return;
    }
    if let Some(result) = scan_of_index(index) {
        write_sidecar_atomic(&sidecar, &fingerprint, &result);
    }
}

/// Read a package's cached phantom verdict, or SCAN it on-demand (and cache the
/// result) when no sidecar exists yet. The link-time CONSUMER
/// ([`crate::pm_engine::phantom_closure`]) calls this so its eject decision is
/// correct REGARDLESS of whether a sidecar was pre-written.
///
/// Why the on-demand scan is load-bearing (the warm-cache-first-install gap):
/// the extract hook writes a sidecar only on a genuine tarball FETCH, so a package
/// WARM in the CAS with no sidecar (GC'd, or cached by a pre-eject-default nub)
/// reaches link with no verdict. Treating that as "no eject" left the package
/// symlinked to the shared store and its undeclared phantom 404'd (`nuxt prepare`
/// → `Cannot find package 'scule'`). Scanning here at the link-time decision point
/// — where the resolved graph and loaded CAS index are both in hand — closes it
/// for every path at once (install/add/update, first or Nth, missing or corrupt
/// sidecar).
///
/// Best-effort like the producer: a torn/corrupt sidecar is treated as a miss;
/// only an unavailable or failed scan degrades to "no eject", never a crash or a
/// false break. The write-on-scan reuses [`scan_and_cache`]'s atomic publish, so
/// a subsequent install hits the warm sidecar when publication succeeds.
pub(crate) fn cached_or_scan_verdict(dir: &Path, index: &PackageIndex) -> Option<ScanResult> {
    let fingerprint = index_content_fingerprint(index);
    let sidecar = sidecar_path(dir, &fingerprint);
    if let Ok(bytes) = std::fs::read(&sidecar)
        && let Ok(result) = serde_json::from_slice::<ScanResult>(&bytes)
    {
        return Some(result);
    }
    // No (or unreadable) sidecar → scan the already-loaded index now, cache it,
    // and use the verdict for this install's eject decision.
    let result = scan_of_index(index)?;
    write_sidecar_atomic(&sidecar, &fingerprint, &result);
    Some(result)
}

/// Scan a package's CAS-backed index into a [`ScanResult`], panic-guarded.
///
/// Panic-safety rests on the scan being panic-free BY CONSTRUCTION, not on the
/// `catch_unwind`: oxc reports an unparseable/hostile file via a return flag (not
/// an unwind), `serde`/`fs` return `Result`, and the graph walk is depth- and
/// size-bounded — so a crafted tarball degrades to a scan miss, never a crash.
/// The `catch_unwind` is a redundant guard that only engages under an unwinding
/// profile (dev/test); the shipped release profile is `panic = "abort"`, where it
/// is inert. Do not treat it as a production safety net.
fn scan_of_index(index: &PackageIndex) -> Option<ScanResult> {
    // `StoredFile.store_path` is the absolute CAS blob; the scanner resolves the
    // reachable graph over the relpath key set and reads content from the blobs.
    let files: Vec<(String, PathBuf)> = index
        .iter()
        .map(|(rel, file)| (rel.clone(), file.store_path.clone()))
        .collect();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_index(&files)))
        .ok()
        .flatten()
}

/// Persist a scan verdict to its per-content sidecar via an atomic temp+rename.
///
/// The serialized JSON is the [`nub_phantom_scan::ScanResult`], read back by the
/// CONSUMER ([`crate::pm_engine::phantom_closure`]) — which, being in nub-cli,
/// deserializes it into the typed `ScanResult` (no cross-fork string coupling).
/// Best-effort: any fs failure leaves the sidecar absent or unchanged (a later
/// read retries an absent/corrupt target as a miss). Shared by the extract-hook
/// producer ([`scan_and_cache`]) and the link-time [`cached_or_scan_verdict`] so
/// both publish identically.
fn write_sidecar_atomic(sidecar: &Path, fingerprint: &str, result: &ScanResult) {
    // The versioned subdir (`sidecar`'s parent) is where both the temp and the
    // final sidecar live, so the atomic rename stays within one directory.
    let Some(subdir) = sidecar.parent() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(result) else {
        return;
    };
    let _ = std::fs::create_dir_all(subdir);
    // Atomic publish: write a per-call-unique temp then rename, so a concurrent
    // installer's linker never observes a half-written sidecar. (The reader
    // treats a torn read as a miss and retries the scan, but rename closes the
    // window.) The temp name carries the pid AND a process-wide sequence: two
    // rayon tasks scanning the SAME content fingerprint — an
    // npm-alias and its real package can share one CAS index — must not write the
    // same temp path. Concurrent renames of the same content to the same target
    // are last-writer-wins and byte-identical.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = subdir.join(format!("{fingerprint}.{}.{seq}.tmp", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, sidecar).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Nub's CAS store schema dir: `<nub-data>/store/v1/`, the parent of the CAS
/// `files/` and `index/` tiers and the `phantom/` sidecar tier. Derives from the
/// SAME [`crate::pm_engine::nub_data_dir`] nub configures its `storeDir` setting
/// from (`nub_data_dir()/store`), plus aube's `v1/` schema suffix — so the store
/// handle and the sidecar dir share ONE base with the real store and cannot drift
/// (the XDG resolution is not re-implemented here). `None` when no data home
/// resolves. `pub(crate)` so the sidecar CONSUMER
/// ([`crate::pm_engine::phantom_closure`]) derives its store handle from the same
/// base this producer uses.
pub(crate) fn store_v1_dir() -> Option<PathBuf> {
    Some(crate::pm_engine::nub_data_dir()?.join("store/v1"))
}

/// The per-content sidecar directory: `<nub-data>/store/v1/phantom/`, next to the
/// CAS + index tiers. `None` when no data home resolves (the scanner then simply
/// doesn't arm). `pub(crate)` so the consumer reads the same directory this
/// producer writes.
pub(crate) fn phantom_cache_dir() -> Option<PathBuf> {
    Some(store_v1_dir()?.join("phantom"))
}

/// The phantom scanner's LOGIC version — BUMP on ANY change to the scanner's
/// detection logic (`nub-phantom-scan`'s extract / specifier / classify passes:
/// a new heuristic like R3 createRequire/template detection, a changed
/// classification, a fixed miss). Sidecars are keyed by content fingerprint AND
/// this version (it is a path segment, see [`sidecar_path`]), so a version's
/// immutable bytes — which hash to the same fingerprint forever — are re-scanned
/// after a bump instead of serving a stale verdict: the bump makes every prior
/// sidecar's path unreachable, so the extract hook or link-time scan writes a
/// fresh verdict under the new version and the old-version sidecars are ignored
/// (GC-able). Forgetting to bump when the logic changes reintroduces the exact
/// forward-compat gap this segment closes. Starts at 1 for the post-R3
/// scanner: there is no prior VERSIONED scheme to migrate from, and any
/// pre-versioning flat `phantom/<fingerprint>.json` sidecar (only ever written
/// into ephemeral dev/CI caches — the eject default has not shipped in a release)
/// is unreachable from the `s<N>/` path, so it is ignored and simply re-scanned.
///
/// Bumping this is a COMPLETE forward-compat fix, not a half one: it is folded
/// into [`settings_fingerprint`] (the install-state token), so a bump both
/// re-scans content into a new sidecar path AND invalidates the warm tree — the
/// link phase re-runs and the consumer applies the new-version verdicts. Without
/// that fold a bump would re-scan but never re-materialize a warm tree (aube
/// skips link on an unchanged lockfile + flag), silently no-op'ing the
/// improvement. The relink a bump forces is a one-time cost on the next install
/// after upgrade; harmless and expected (the whole point is to pick up the better
/// verdict). Just bump the number when the scanner logic changes — the coupling
/// is structural, nothing else to remember.
pub(crate) const PHANTOM_SCANNER_VERSION: u32 = 2;

/// THE single source of truth for a phantom sidecar's location: the versioned
/// subdir `<phantom_cache_dir>/s<PHANTOM_SCANNER_VERSION>/<fingerprint>.json`.
/// Both halves derive their path HERE — the extract-time PRODUCER
/// ([`scan_and_cache`]) and the link-time CONSUMER
/// ([`crate::pm_engine::phantom_closure`]) — so the fingerprint keying, the
/// `.json` extension, AND the scanner-version segment stay in lockstep and cannot
/// drift apart (a producer/consumer path disagreement would silently serve "no
/// eject" for every package). `base` is the caller-resolved
/// [`phantom_cache_dir`]; the version subdir keeps each scanner generation's
/// sidecars grouped for wholesale GC of a superseded version.
pub(crate) fn sidecar_path(base: &Path, fingerprint: &str) -> PathBuf {
    base.join(format!("s{PHANTOM_SCANNER_VERSION}"))
        .join(format!("{fingerprint}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidecar path MUST carry the scanner-version segment ahead of the
    /// fingerprint file, so a version bump relocates every sidecar (making the old
    /// verdict unreachable → a re-scan). Both halves derive through this one helper,
    /// so asserting the format here pins the contract they share.
    #[test]
    fn sidecar_path_carries_scanner_version_segment() {
        let base = Path::new("/store/v1/phantom");
        let got = sidecar_path(base, "deadbeef");
        assert_eq!(
            got,
            base.join(format!("s{PHANTOM_SCANNER_VERSION}"))
                .join("deadbeef.json")
        );
    }

    /// The user (enabled) token folds the scanner version so a bump invalidates a
    /// warm tree and forces a re-scan/relink; the dead on/off toggle is gone, so the
    /// token is just the scanner segment. The disabled token (reachable only via the
    /// internal A/B seam) is version-free and distinct, so flipping the seam still
    /// re-links to the pure-symlink shape. Pins both against a future refactor.
    #[test]
    fn enabled_token_folds_version_disabled_seam_token_is_distinct() {
        assert_eq!(
            settings_token(true),
            format!("phantom_scanner={PHANTOM_SCANNER_VERSION}")
        );
        assert_eq!(settings_token(false), "phantom_eject=disabled");
        assert_ne!(settings_token(true), settings_token(false));
    }

    /// The internal disable seam's truthiness contract: only an explicit truthy
    /// value turns eject off; unset, empty, `0`, and any other string keep it ON.
    /// This is the sole off-switch — there is no user knob — so a wrong parse here
    /// would either strand the A/B control or hand users a hidden opt-out.
    #[test]
    fn internal_seam_only_truthy_disables_eject() {
        for on in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
        ] {
            assert!(!eject_disabled(on), "eject must stay ON for {on:?}");
        }
        for off in [Some("1"), Some("true"), Some("YES"), Some(" on ")] {
            assert!(eject_disabled(off), "internal seam disables for {off:?}");
        }
    }

    /// The warm-cache-first-install fix: the link-time consumer must SCAN a
    /// package on-demand when its sidecar is missing or corrupt, cache the result,
    /// then serve the repaired cache without reading CAS blobs. Before the fix a
    /// warm-cached phantom package with no sidecar stayed symlinked and its phantom
    /// 404'd (`nuxt prepare` → `Cannot find package 'scule'`). Exercises the real
    /// store-IO path: a CAS `PackageIndex` built from a package that statically
    /// imports an UNDECLARED dependency.
    #[test]
    fn cached_or_scan_verdict_scans_on_missing_or_corrupt_sidecar_then_serves_cache() {
        use aube_store::Store;
        let base = std::env::temp_dir().join(format!(
            "nub-cached-verdict-{}-{}",
            std::process::id(),
            // per-call unique so parallel tests don't share the fixture
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"demo","main":"index.js","dependencies":{"declared":"1"}}"#,
        )
        .unwrap();
        // Reachable main-graph code: a declared dep + an UNDECLARED phantom.
        std::fs::write(
            pkg.join("index.js"),
            "import 'undeclared-phantom'; const d = require('declared');",
        )
        .unwrap();

        let store = Store::at(base.join("store/files"));
        let index = store.import_directory(&pkg).unwrap();
        let sidecar_dir = base.join("phantom");
        let sidecar = sidecar_path(&sidecar_dir, &index_content_fingerprint(&index));

        // No sidecar yet — the extract hook did not run for this warm CAS entry.
        // The consumer must scan on-demand.
        assert!(!sidecar.exists(), "precondition: no sidecar written yet");
        let v =
            cached_or_scan_verdict(&sidecar_dir, &index).expect("scan-on-miss yields a verdict");
        assert!(
            v.has_unguarded_phantom,
            "the undeclared import must be flagged on the scan-on-miss path"
        );
        assert!(
            v.targets.iter().any(|t| t.name == "undeclared-phantom"),
            "the phantom target is recorded: {:?}",
            v.targets
        );
        assert!(
            sidecar.exists(),
            "the on-demand scan is cached for the next warm hit"
        );

        // A corrupt sidecar is treated like a miss, rescanned while the CAS blobs
        // are available, and atomically replaced with valid JSON.
        std::fs::write(&sidecar, b"not-json").unwrap();
        let repaired = cached_or_scan_verdict(&sidecar_dir, &index)
            .expect("a corrupt sidecar is rescanned and repaired");
        assert!(
            repaired.has_unguarded_phantom
                && repaired
                    .targets
                    .iter()
                    .any(|t| t.name == "undeclared-phantom"),
            "the corrupt-sidecar rescan preserves the phantom verdict"
        );
        let repaired_bytes = std::fs::read(&sidecar).unwrap();
        assert!(
            serde_json::from_slice::<ScanResult>(&repaired_bytes).is_ok(),
            "the corrupt sidecar is replaced with valid JSON"
        );

        // The next call SERVES THE REPAIRED CACHE rather than rescanning:
        // destroying the CAS blobs leaves only the cached verdict (the fingerprint
        // is a pure function of the in-memory index).
        let _ = std::fs::remove_dir_all(base.join("store"));
        let v2 = cached_or_scan_verdict(&sidecar_dir, &index).expect("cached verdict served");
        assert!(
            v2.has_unguarded_phantom && v2.targets.iter().any(|t| t.name == "undeclared-phantom"),
            "the repaired sidecar is served without rescanning"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
