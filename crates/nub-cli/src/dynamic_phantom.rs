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
//! Placement — EXTRACT TIME, not post-link. The scan fires at the end of each
//! tarball import, on the fetch/blocking fan-out thread, so per-version analysis
//! OVERLAPS the network-bound fetch phase (the scan CPU hides under fetch's idle
//! cores) instead of adding a serial post-link pass. Each verdict is written to a
//! per-content sidecar.
//!
//! This module holds the engine-agnostic PRIMITIVES — the scan itself
//! ([`scan_and_cache_files`]), the verdict read-or-scan
//! ([`cached_or_scan_verdict_files`]), and the sidecar path they share. Wiring
//! them to an engine's extract and materialize seams is
//! [`crate::pm_engine::phantom_hooks`]'s job: it registers the observer that
//! drives the producer and the policy that reads the sidecars back, seeding the
//! selective-subtree closure with each flagged importer so a poisoned version is
//! ejected project-local. Both halves build the sidecar path through the single
//! [`sidecar_path`] helper, so the fingerprint keying and the scanner-version
//! segment cannot drift apart. The sidecar path folds
//! [`PHANTOM_SCANNER_VERSION`] so a scanner-logic improvement re-scans already
//! cached content instead of serving the stale verdict its immutable bytes would
//! otherwise key forever.
//!
//! Under the internal A/B seam ([`enabled`] returns false) both seams no-op — no
//! sidecars written, none consulted — so the install path is byte-identical to a
//! build without the scanner (a pure-symlink tree).

use std::path::{Path, PathBuf};

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
/// [`PHANTOM_SCANNER_VERSION`] plus the curated-eject list token
/// ([`crate::pm_engine::phantom_closure::project_context_eject_token`]). The scanner
/// fold makes a scanner-logic bump COMPLETE rather than a half-fix — the bump
/// re-scans content into a new sidecar path, but on a warm tree with an unchanged
/// lockfile aube would SKIP the link phase and never apply the improved verdict;
/// changing this token forces the link to re-run so the consumer picks up the
/// new-version sidecars. The curated-eject fold does the same for the #457 list: its
/// members are injected inside the expand hook, past aube's `disk_materialize_packages`
/// settings fold, so folding the list token here is what invalidates a warm tree on
/// the initial ship AND on any future list edit (else the stale symlinked shape is
/// accepted and #457 stays unfixed on existing installs). It also folds
/// [`GVS_EJECT_ALGO_VERSION`], which covers the third way a warm tree goes stale:
/// the plan is unchanged but the LINKER writes it differently (nub#711).
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
        format!(
            "phantom_scanner={PHANTOM_SCANNER_VERSION};project_context={};gvs_eject_algo={GVS_EJECT_ALGO_VERSION}",
            crate::pm_engine::phantom_closure::project_context_eject_token()
        )
    } else {
        "phantom_eject=disabled".to_string()
    }
}

/// Scan one freshly-imported package and persist its verdict to the per-content
/// sidecar, over the store already reduced to what the scanner reads: each
/// file's path inside the package and the content-addressed blob holding it.
/// The engine-shaped half of producing that pair is the caller's.
pub(crate) fn scan_and_cache_files(dir: &Path, fingerprint: &str, files: &[(String, PathBuf)]) {
    let sidecar = sidecar_path(dir, fingerprint);
    // Cross-process / warm cache hit: this exact content was already scanned
    // UNDER THE CURRENT SCANNER VERSION (the version is in `sidecar`'s path, so a
    // scanner bump makes this `exists()` false and forces a re-scan). The verdict
    // is a pure function of the immutable bytes + scanner logic, so a concurrent
    // first-writer race is benign (identical result) — skip the redundant scan.
    if sidecar.exists() {
        return;
    }
    if let Some(result) = scan_of_files(files) {
        write_sidecar_atomic(&sidecar, fingerprint, &result);
    }
}

/// A content fingerprint over what the store says a package holds: each
/// file's path, the digest of its contents, and whether it is executable.
///
/// The scheme matches the one aube's store computes, but the digests do not
/// — the two engines hash contents differently — so the same package under
/// each engine keys a different sidecar. That costs a cold scan once and
/// nothing else: a verdict is a pure function of the bytes, and the two
/// engines do not share a store to begin with.
pub(crate) fn content_fingerprint<'a>(
    entries: impl Iterator<Item = (&'a str, &'a str, bool)>,
) -> String {
    let mut entries: Vec<(&str, &str, bool)> = entries.collect();
    entries.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for (path, digest, executable) in entries {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.as_bytes());
        hasher.update(if executable { b"\x01" } else { b"\x00" });
    }
    hasher.finalize().to_hex().to_string()
}

/// Read a package's cached phantom verdict, or SCAN it on-demand (and cache the
/// result) when no sidecar exists yet, over the file list the engine produces —
/// which is the whole of what the scan reads. The engine-shaped half is the
/// caller's.
///
/// Why the on-demand scan is load-bearing (the warm-cache-first-install gap):
/// the extract hook writes a sidecar only on a genuine tarball FETCH, so a package
/// WARM in the CAS with no sidecar (GC'd, or cached by a pre-eject-default nub)
/// reaches link with no verdict. Treating that as "no eject" left the package
/// symlinked to the shared store and its undeclared phantom 404'd (`nuxt prepare`
/// → `Cannot find package 'scule'`).
///
/// Best-effort like the producer: a torn/corrupt sidecar is treated as a miss;
/// only an unavailable or failed scan degrades to "no eject", never a crash or a
/// false break.
pub(crate) fn cached_or_scan_verdict_files(
    dir: &Path,
    read_fallback_dir: Option<&Path>,
    fingerprint: &str,
    files: &[(String, PathBuf)],
) -> Option<ScanResult> {
    let sidecar = sidecar_path(dir, fingerprint);
    // `read_fallback_dir` is the global store's sidecar tier when installs
    // are writing a project-local store: its verdicts are read, never
    // written, the same layering the CAS itself uses.
    let cached = std::iter::once(sidecar.clone())
        .chain(read_fallback_dir.map(|dir| sidecar_path(dir, fingerprint)));
    for candidate in cached {
        if let Ok(bytes) = std::fs::read(&candidate)
            && let Ok(result) = serde_json::from_slice::<ScanResult>(&bytes)
        {
            return Some(result);
        }
    }
    // No (or unreadable) sidecar → scan the already-loaded index now, cache it,
    // and use the verdict for this install's eject decision.
    let result = scan_of_files(files)?;
    write_sidecar_atomic(&sidecar, fingerprint, &result);
    Some(result)
}

/// Scan a package into a [`ScanResult`], panic-guarded, over the file list
/// both engines produce — each file's path inside the package and the
/// content-addressed blob holding it, which is the whole of what the scan
/// reads.
///
/// Panic-safety rests on the scan being panic-free BY CONSTRUCTION, not on the
/// `catch_unwind`: oxc reports an unparseable/hostile file via a return flag (not
/// an unwind), `serde`/`fs` return `Result`, and the graph walk is depth- and
/// size-bounded — so a crafted tarball degrades to a scan miss, never a crash.
/// The `catch_unwind` is a redundant guard that only engages under an unwinding
/// profile (dev/test); the shipped release profile is `panic = "abort"`, where it
/// is inert. Do not treat it as a production safety net.
fn scan_of_files(files: &[(String, PathBuf)]) -> Option<ScanResult> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_index(files)))
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
/// producer ([`scan_and_cache_files`]) and the link-time
/// [`cached_or_scan_verdict_files`] so both publish identically.
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

/// Nub's CAS store schema dirs: `<store-root>/v1/`, the parent of the CAS
/// `files/` and `index/` tiers and the `phantom/` sidecar tier — the one the
/// engine WRITES this run, plus the read-only global one it still reads when
/// the default store is unwritable (a coding agent's sandbox). Resolves through
/// [`aube::commands::resolved_project_store_v1_dirs`], the engine's own
/// `storeDir` resolution and fallback decision, anchored at the walked-up
/// project/workspace root — so a configured `store-dir` override moves the
/// sidecar tier WITH the store it indexes (#643), and a project-local fallback
/// store carries its own sidecars. The ANCHOR is the load-bearing half:
/// `.npmrc` and `pnpm-workspace.yaml` discovery does not walk up, and the
/// install pipeline anchors at `workspace_or_project_root()`, so resolving
/// against the raw process cwd instead would miss the override for every
/// command run from inside a workspace member and silently return the default
/// store. Falls back to nub's [`crate::pm_engine::nub_data_dir`] — the same
/// base its `storeDir` embedder default is built from — when no project root
/// resolves at all. `None` when no data home resolves either. `pub(crate)` so
/// the sidecar CONSUMER ([`crate::pm_engine::phantom_closure`]) derives its
/// store handle from the same dirs this producer uses.
pub(crate) fn store_v1_dirs() -> Option<aube::commands::StoreV1Dirs> {
    if let Some(dirs) = aube::commands::resolved_project_store_v1_dirs() {
        return Some(dirs);
    }
    Some(aube::commands::StoreV1Dirs {
        primary: crate::pm_engine::nub_data_dir()?.join("store/v1"),
        read_fallback: None,
    })
}

/// The per-content sidecar directory the producer WRITES: `<store>/v1/phantom/`
/// under the primary store, next to the CAS + index tiers. `None` when no data
/// home resolves (the scanner then simply doesn't arm). `pub(crate)` so the
/// consumer writes on-demand verdicts to the same directory this producer does;
/// the consumer additionally READS the global store's sidecars through
/// [`store_v1_dirs`]'s fallback.
pub(crate) fn phantom_cache_dir() -> Option<PathBuf> {
    Some(store_v1_dirs()?.primary.join("phantom"))
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
pub(crate) const PHANTOM_SCANNER_VERSION: u32 = 5;

/// Version of what the linker's GVS-populate pass WRITES TO DISK for a given eject
/// set — bumped when the same plan produces a different on-disk shape.
///
/// Distinct from [`PHANTOM_SCANNER_VERSION`] (which plan is computed) and from
/// aube's `disk_materialize_packages` fold (which NAMES are in the seed): both of
/// those are unchanged when only the EXECUTOR changes, so neither invalidates.
/// nub#711 is the case in point — `link_workspace` never consulted the eject set,
/// so every workspace install produced an all-symlinks tree. Fixing the linker
/// moves no hash: the lockfile, the manifest, the settings and the seed are all
/// identical, so `try_install_fast_path` reports "Already up to date" and the
/// broken layout survives the upgrade. Only the users who filed the bug have such
/// a tree, so without this salt the fix reaches nobody until unrelated churn
/// (a lockfile edit, `--force`) happens to bust the state.
///
/// Same shape and same remedy as aube's `hoisted_layout_algo` salt, which exists
/// because a hoisted-layout algorithm change likewise left the graph hash
/// identical. Bump on any future change to what that pass materializes.
///
/// COST of a bump, measured rather than assumed: the dependency side is cheap —
/// no refetch, no rebuild, no side-effects-cache bust, no lockfile churn, since
/// those all stay gated on content-hash deltas. But root lifecycle hooks are
/// gated only on the fast path being missed, so `preinstall` and
/// `install`/`postinstall`/`prepare` re-run ONCE PER IMPORTER on the first
/// install after a bump — meaningful in a workspace whose members drive builds
/// from `prepare`. Accepted here: the alternative is leaving every already-installed
/// workspace on the broken layout, and `PHANTOM_SCANNER_VERSION` bumps already
/// carry the same cost. Narrowing the salt to "only when the eject closure is
/// non-empty" is NOT available — the closure needs the resolved graph, and this
/// hash is computed before resolution. The auto-install path (`nub run`) does not
/// pay it at all: it passes no CLI flags, which skips the settings-hash check.
pub(crate) const GVS_EJECT_ALGO_VERSION: u32 = 1;

/// THE single source of truth for a phantom sidecar's location: the versioned
/// subdir `<phantom_cache_dir>/s<PHANTOM_SCANNER_VERSION>/<fingerprint>.json`.
/// Both halves derive their path HERE — the extract-time PRODUCER
/// ([`scan_and_cache_files`]) and the link-time CONSUMER
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

    /// The user (enabled) token folds the scanner version, the curated-eject list
    /// token AND the GVS-eject algorithm version, so a scanner bump, a #457 list edit,
    /// or a change to what the linker MATERIALIZES (nub#711) each invalidates a warm
    /// tree and forces a re-scan/relink; the dead on/off toggle is gone. The disabled
    /// token (reachable only via the internal A/B seam) is version-free and distinct,
    /// so flipping the seam still re-links to the pure-symlink shape. Pins both
    /// against a future refactor.
    #[test]
    fn enabled_token_folds_version_disabled_seam_token_is_distinct() {
        assert_eq!(
            settings_token(true),
            format!(
                "phantom_scanner={PHANTOM_SCANNER_VERSION};project_context={};gvs_eject_algo={GVS_EJECT_ALGO_VERSION}",
                crate::pm_engine::phantom_closure::project_context_eject_token()
            )
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
}
