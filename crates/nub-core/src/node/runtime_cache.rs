//! Single-binary runtime extraction (the `embed-runtime` feature only).
//!
//! In single-binary mode the whole `runtime/` tree (preload scripts + vendored
//! `node_modules` + the platform `nub-native.node`) is embedded in the binary as
//! a zstd-19 tar blob (see `build.rs`). On first run we inflate it ONCE to a
//! versioned cache dir and hand `find_preload` that path; every later run finds
//! the dir already present and pays a single `stat`.
//!
//! Design points that make this safe:
//!
//! - **Atomic publish, no lock.** We extract into a unique `.<key>.<pid>.<rand>.tmp`
//!   dir then `rename` it onto `<cache>/runtime-<key>/`. `rename` of a populated
//!   dir is atomic, so a concurrent reader sees the complete dir or nothing. If a
//!   sibling won the race (target already exists) the loser removes its tmp and
//!   uses the winner's dir. No flock, no partial-population window.
//!
//! - **R1 — safe, per-user extraction base (access-control front-line).** The base
//!   is created `0700` and, before a PRE-EXISTING base is adopted, validated:
//!   owner == current euid, no group/world write bit, not a symlink (unix). A base
//!   that fails validation is NOT used and NOT destroyed (we don't own it) — we
//!   skip to the next candidate, recovering rather than bricking. The `$TMPDIR`
//!   fallback is per-user (`$TMPDIR/nub-<uid>`), so a shared world-writable `/tmp`
//!   can't host a base another user planted into. On a 0700 owner-only base in a
//!   sticky `/tmp`, a cross-uid attacker can neither write inside it nor rename it
//!   away, which also closes the verify→load (TOCTOU) window for any principal but
//!   the user themselves (same-uid is already game-over — they own the binary).
//!   Windows: `%USERPROFILE%\.cache` / `%TEMP%` are per-user ACL'd by the OS, so
//!   there is no shared-temp planted-dir class; the unix stat checks are skipped.
//!
//! - **R2 — verify the loaded code against a baked-in hash (integrity backstop).**
//!   `build.rs` bakes the BLAKE3 digest of the four directly-loaded entrypoints
//!   (`preload.mjs`, `preload.cjs`, `watch-env-guard.cjs`, and
//!   `addons/nub-native.node`) into the binary. On
//!   the load path (once per process, inside the OnceLock init, ~6 ms for the ~9 MB
//!   addon on aarch64 — BLAKE3 over software SHA-256's ~28 ms there) the EXTRACTED
//!   entrypoints are re-hashed against those consts. On mismatch we
//!   SELF-HEAL: re-extract the trusted in-binary blob over the dir and re-verify —
//!   silent success means the on-disk copy was stale/corrupt/tampered and we
//!   replaced it with the trusted bytes. A PERSISTENT mismatch (still wrong after a
//!   clean re-extract: a hashing bug, or a writer racing the extraction) is a
//!   genuine anomaly: under the default canary mode it emits a NON-FATAL warning to
//!   stderr and PROCEEDS (a verify-on-load bug must never brick nub on day one);
//!   flip [`INTEGRITY_ENFORCE`] to fail closed once the wild mismatch rate is ~0.
//!
//! - **Age-based GC.** After a fresh extract, sibling `runtime-*` dirs older than
//!   30 days are removed (best-effort). Age-based, not "delete all non-current",
//!   so two versions in active use (a global install + an `npx nub@<old>`) don't
//!   evict each other. In-progress `.tmp` dirs and the current dir are never
//!   touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::discovery;

/// The embedded blob: `runtime/` tarred and zstd-19 compressed at build time.
static RUNTIME_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.tar.zst"));

/// `runtime-<pkg version>-<blobhash8>` — a compile-time literal baked by build.rs.
const CACHE_KEY: &str = env!("NUB_RUNTIME_CACHE_KEY");

/// R2: per-entrypoint BLAKE3 digest (hex), baked by build.rs from the staged
/// runtime. These verify the EXTRACTED files on the load path; they live inside the
/// (signed) binary so a tampered on-disk file can't swap its own hash alongside it.
const HASH_PRELOAD_MJS: &str = env!("NUB_RUNTIME_HASH_PRELOAD_MJS");
const HASH_PRELOAD_CJS: &str = env!("NUB_RUNTIME_HASH_PRELOAD_CJS");
const HASH_WATCH_ENV_GUARD: &str = env!("NUB_RUNTIME_HASH_WATCH_ENV_GUARD");
const HASH_ADDON: &str = env!("NUB_RUNTIME_HASH_ADDON");

/// The four directly-loaded entrypoints and their baked digests. The native addon
/// (`dlopen`'d) and the preload scripts (`--require`d/`--import`ed) are the actual code-load
/// surface; the vendored `node_modules` polyfills are intentionally OUT of the
/// per-load hash (R1's 0700 owner-only base already closes their planted-file
/// vector, and hashing the whole ~13 MB tree every run would be a real regression
/// for a fast script runner — the entrypoints keep the cost ~1-2 ms).
const VERIFIED_ENTRYPOINTS: [(&str, &str); 4] = [
    ("preload.mjs", HASH_PRELOAD_MJS),
    ("preload.cjs", HASH_PRELOAD_CJS),
    ("watch-env-guard.cjs", HASH_WATCH_ENV_GUARD),
    ("addons/nub-native.node", HASH_ADDON),
];

/// Fail-closed switch for R2. `false` = CANARY: a persistent post-re-extract
/// integrity mismatch warns and PROCEEDS (a verify-on-load bug must not brick nub).
/// `true` = ENFORCE: refuse to load on a persistent mismatch. Ship canary, watch for
/// real-world warning reports, then flip this one line to `true` once the wild
/// mismatch rate is confirmed ~0. (Self-heal — re-extracting the trusted blob over a
/// stale/corrupt/tampered on-disk copy — runs in BOTH modes; this only governs the
/// terminal decision when a FRESH extraction still fails to verify.)
const INTEGRITY_ENFORCE: bool = false;

/// Stale-version eviction threshold.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Memoized result of the (at most once) extraction for this process.
static EXTRACTED: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Ensure the embedded runtime is extracted and return the dir holding
/// `preload.mjs` / `addons/` / `node_modules/`. Runs the work at most once per
/// process (OnceLock), returning a cheap clone afterward. `None` only on a
/// genuinely unusable environment (no writable cache dir) — the caller then runs
/// without augmentation, exactly as it would for a not-found sidecar.
pub fn ensure_runtime() -> Option<PathBuf> {
    EXTRACTED.get_or_init(extract_once).clone()
}

fn extract_once() -> Option<PathBuf> {
    extract_with(&base_candidates())
}

/// The candidate-driven core of [`extract_once`], split out so tests can drive it
/// with controlled bases (e.g. an unsafe base followed by a safe one — proving R1
/// recovers rather than bricks).
fn extract_with(candidates: &[PathBuf]) -> Option<PathBuf> {
    // Warm path: a SAFE candidate already holds the extracted dir. Each base is
    // owner/perms-validated (R1) BEFORE its cached dir is trusted, and the dir's
    // entrypoints are verified (R2) before adoption. `preload.mjs` existence is the
    // cheap completeness pre-check (one stat) before paying for the hashes — the dir
    // only ever appears via the atomic rename of a fully-unpacked tree, so a missing
    // sentinel means re-extract, not trust.
    for base in candidates {
        let Some(safe_base) = ensure_safe_base(base) else {
            continue;
        };
        let target = safe_base.join(CACHE_KEY);
        if target.join("preload.mjs").is_file() {
            if let Some(dir) = verify_or_heal(&safe_base, &target, false) {
                return Some(dir);
            }
            // `None` here is the ENFORCE refusal for THIS base (a fresh re-extract
            // still failed to verify). Re-extracting into another base would produce
            // the same bytes and the same verdict, so don't fall through to a cold
            // extract — try the next candidate's warm cache, then give up.
            continue;
        }
    }

    // Cold path: extract into the first SAFE, writable base. `try_extract` probes
    // writability by creating its tmp dir, so a read-only primary falls through.
    for base in candidates {
        let Some(safe_base) = ensure_safe_base(base) else {
            continue;
        };
        if let Some((dir, self_extracted)) = try_extract(&safe_base) {
            // A dir WE just extracted should always verify; a mismatch means a
            // build.rs hashing bug (self_extracted ⇒ already_fresh ⇒ no pointless
            // re-extract, straight to the canary/enforce decision). But if we ADOPTED
            // a concurrent winner's dir (rename lost the race), treat it as warm
            // (already_fresh=false) so a winner corrupted after publish still self-heals.
            return verify_or_heal(&safe_base, &dir, self_extracted);
        }
    }

    eprintln!(
        "nub: could not extract the embedded runtime (no writable cache dir); \
         set XDG_CACHE_HOME to a writable path"
    );
    None
}

/// Candidate cache bases in priority order: `~/.cache/nub` (or `$XDG_CACHE_HOME/nub`)
/// then the per-user `$TMPDIR/nub-<uid>`. Deduplicated so an exotic
/// `TMPDIR == cache_dir` setup doesn't try the same path twice.
fn base_candidates() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Some(c) = discovery::cache_dir() {
        out.push(c);
    }
    let tmp = std::env::temp_dir().join(tmp_subdir_name());
    if !out.contains(&tmp) {
        out.push(tmp);
    }
    out
}

/// The `$TMPDIR` fallback subdir name. Per-user on unix (`nub-<euid>`) so a shared,
/// world-writable `/tmp` can't host a base another user planted into — even before
/// the owner validation runs. Windows `%TEMP%` is already per-user ACL'd, so there's
/// no uid to scope by.
#[cfg(unix)]
fn tmp_subdir_name() -> String {
    format!("nub-{}", current_euid())
}
#[cfg(not(unix))]
fn tmp_subdir_name() -> String {
    "nub".to_string()
}

/// R1: resolve `base` to a SAFE per-user dir, or `None` if it can't be made/validated
/// safe (the caller then recovers by trying the next candidate — never bricks).
///
/// - If absent: create the leaf `0700` ATOMICALLY (no umask window where the base is
///   briefly world-writable — see [`create_base_dir`]). This is also the writability
///   probe (a read-only FS fails here → `None` → next candidate).
/// - Validate ownership/perms (unix) even on a dir we just "created": creation tolerates
///   an attacker who pre-created the path (`EEXIST`), so the POST-create owner check is
///   what actually rejects a planted base. A failed validation returns `None` — we
///   neither use nor destroy a dir we don't own.
fn ensure_safe_base(base: &Path) -> Option<PathBuf> {
    if !base.exists() && create_base_dir(base).is_err() {
        return None;
    }
    if is_safe_dir(base) {
        Some(base.to_path_buf())
    } else {
        None
    }
}

/// Create `base` (and any missing parents) with the leaf at `0700`, atomically on
/// unix — `DirBuilder::mode` applies the mode in the `mkdir` syscall itself, so there
/// is no window (as a `create_dir_all` + `chmod` pair has under `umask 000`) where the
/// leaf is world-writable and a cross-uid attacker could plant into it. `EEXIST` is
/// tolerated (the caller's `is_safe_dir` then rejects a pre-created/planted base on the
/// owner check). On non-unix, mode is a no-op and `%USERPROFILE%`/`%TEMP%` ACLs apply.
#[cfg(unix)]
fn create_base_dir(base: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = base.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}
#[cfg(not(unix))]
fn create_base_dir(base: &Path) -> std::io::Result<()> {
    fs::create_dir_all(base)
}

/// Extract the blob into `<base>/runtime-<key>/` via a unique tmp dir + atomic
/// rename. The caller has already [`ensure_safe_base`]'d `base`. Returns
/// `(dir, self_extracted)` — `self_extracted` is `true` if WE published the dir,
/// `false` if we adopted a concurrent winner's — or `None` if the extraction failed.
fn try_extract(base: &Path) -> Option<(PathBuf, bool)> {
    let target = base.join(CACHE_KEY);

    let tmp = unique_tmp(base);
    // A leftover tmp from a crashed run with the same name is vanishingly unlikely
    // (pid + monotonic-ish rand), but clear it so create + unpack start clean.
    let _ = fs::remove_dir_all(&tmp);
    if fs::create_dir_all(&tmp).is_err() {
        return None;
    }
    set_owner_only(&tmp); // target inherits this mode via the rename below

    if let Err(e) = unpack_blob(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        eprintln!("nub: failed to inflate the embedded runtime: {e}");
        return None;
    }

    match fs::rename(&tmp, &target) {
        Ok(()) => {
            gc_stale(base, &target);
            Some((target, true))
        }
        Err(_) => {
            // Either a concurrent extractor already published `target` (the common,
            // benign case — `rename` onto a populated dir fails on both Unix and
            // Windows), or a genuine FS error. Clean up our tmp and adopt the
            // winner's dir if it materialized (marked NOT self-extracted, so the
            // caller re-verifies it as a warm dir).
            let _ = fs::remove_dir_all(&tmp);
            if target.is_dir() {
                Some((target, false))
            } else {
                None
            }
        }
    }
}

/// A unique, hidden tmp dir under `base` for an in-progress extraction.
fn unique_tmp(base: &Path) -> PathBuf {
    base.join(format!(
        ".{CACHE_KEY}.{}.{}.tmp",
        std::process::id(),
        rand_suffix()
    ))
}

// ---- R1 helpers: per-user 0700 base + owner/perms/symlink validation ----------

#[cfg(unix)]
fn current_euid() -> u32 {
    // Safe: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

/// Is `path` a real directory, owned by us, with no group/world write bit, and not a
/// symlink? On non-unix this is just "a real directory" — `%USERPROFILE%`/`%TEMP%`
/// are per-user ACL'd by the OS, so there is no shared-temp planted-dir class the
/// unix checks defend against.
#[cfg(unix)]
fn is_safe_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // symlink_metadata: do NOT traverse a final symlink — a symlinked base is itself
    // the reject (an attacker could point it at a dir they control).
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    // Owner must be us: a dir pre-created under a shared /tmp by another principal is
    // owned by THEM, and adopting it would load their planted files.
    if meta.uid() != current_euid() {
        return false;
    }
    // No group/other write (0o022): a group/world-writable base lets another
    // principal swap our extracted files between verification and load.
    meta.mode() & 0o022 == 0
}
#[cfg(not(unix))]
fn is_safe_dir(path: &Path) -> bool {
    path.is_dir()
}

// ---- R2 helpers: per-entrypoint hash verification + self-heal -----------------

/// Whether to refuse (vs. warn-and-proceed) on a PERSISTENT integrity mismatch.
/// Reads [`INTEGRITY_ENFORCE`] in production; a `#[cfg(test)]` override lets tests
/// exercise the would-be-fatal path without flipping the shipped const.
fn enforce() -> bool {
    #[cfg(test)]
    {
        match TEST_ENFORCE.load(Ordering::Relaxed) {
            1 => return false,
            2 => return true,
            _ => {}
        }
    }
    INTEGRITY_ENFORCE
}

/// Test-only override of [`enforce`]: `0` = use the const, `1` = force off (canary),
/// `2` = force on (enforce).
#[cfg(test)]
static TEST_ENFORCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// BLAKE3 (lowercase hex) of a file's bytes, or `None` if it can't be read.
fn file_blake3_hex(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// Re-hash the extracted entrypoints in `dir` against the baked digests. All four
/// must read AND match. ~6 ms (entrypoints only, addon-dominated), paid at most once
/// per process (the caller runs inside the `EXTRACTED` OnceLock init).
fn verify_entrypoints(dir: &Path) -> bool {
    VERIFIED_ENTRYPOINTS
        .iter()
        .all(|(rel, expected)| file_blake3_hex(&dir.join(rel)).as_deref() == Some(*expected))
}

/// R2 decision for a candidate `target` dir.
///
/// - Verifies clean → adopt it.
/// - Mismatch + `!already_fresh` (a WARM on-disk cache) → SELF-HEAL: re-extract the
///   trusted in-binary blob over `target` and re-verify; success ⇒ adopt the healed
///   dir (the on-disk copy was stale/corrupt/tampered, now trusted).
/// - Persistent mismatch (a fresh re-extract STILL fails, or a cold extraction
///   failed straight away) → genuine anomaly: ENFORCE ⇒ refuse (`None`); CANARY ⇒
///   warn to stderr and proceed with the dir (never brick on a verify-on-load bug).
fn verify_or_heal(base: &Path, target: &Path, already_fresh: bool) -> Option<PathBuf> {
    if verify_entrypoints(target) {
        return Some(target.to_path_buf());
    }

    if !already_fresh {
        // The on-disk cache diverged from the embedded blob — stale (a half-written
        // / AV-quarantined / temp-cleanup-corrupted copy the presence-check trusts),
        // or tampered (a planted file on a base whose perms were somehow bypassed).
        // Either way, re-extract the trusted bytes the binary carries.
        eprintln!(
            "nub: runtime cache at {} did not match the embedded runtime; re-extracting",
            target.display()
        );
        if let Some(healed) = swap_extract(base, target) {
            if verify_entrypoints(&healed) {
                return Some(healed);
            }
        }
    }

    // Persistent: a FRESH extraction from the embedded blob still does not match the
    // baked hashes. That is not a stale-cache story — it's a hashing/build bug or
    // something rewriting the file mid-extraction. Canary by default.
    if enforce() {
        eprintln!(
            "nub: runtime integrity check failed at {} after re-extraction; \
             refusing to load",
            target.display()
        );
        return None;
    }
    // Canary: only proceed if `target` is still a live dir on disk. A failed self-heal
    // can leave it absent (the swap's stale-restore lost the race to gc_stale), and the
    // canary contract is "always hand back a live dir or None" — never a ghost path the
    // child `node` would brick on. A non-existent target degrades to un-augmented.
    if target.is_dir() {
        eprintln!(
            "nub: runtime integrity check failed at {} after re-extraction; proceeding \
             anyway. Please report this at https://github.com/nubjs/nub/issues with your \
             OS and `nub --version`.",
            target.display()
        );
    }
    target.is_dir().then(|| target.to_path_buf())
}

/// Self-heal: re-extract the embedded blob into a fresh tmp and atomically swap it
/// onto `target`, replacing the stale/corrupt/tampered copy. Rare path (only on a
/// verify mismatch). Returns the published dir, or `None` if the re-extract couldn't
/// be written (e.g. the base went read-only).
///
/// The stale dir is moved aside (atomic rename) then the fresh dir is renamed onto
/// the canonical name; a concurrent reader sees the complete old dir, a brief
/// absence, then the complete new dir — never a partial tree. The absence window is
/// same-uid-only (R1's 0700 owner-only base) and triggers at worst a redundant
/// cold-extract in a racing process, which is itself safe.
fn swap_extract(base: &Path, target: &Path) -> Option<PathBuf> {
    let tmp = unique_tmp(base);
    let _ = fs::remove_dir_all(&tmp);
    if fs::create_dir_all(&tmp).is_err() {
        return None;
    }
    set_owner_only(&tmp);
    if let Err(e) = unpack_blob(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        eprintln!("nub: failed to re-inflate the embedded runtime during self-heal: {e}");
        return None;
    }

    let stale = base.join(format!(
        ".stale.{CACHE_KEY}.{}.{}",
        std::process::id(),
        rand_suffix()
    ));
    let moved_aside = fs::rename(target, &stale).is_ok();
    match fs::rename(&tmp, target) {
        Ok(()) => {
            if moved_aside {
                let _ = fs::remove_dir_all(&stale);
            }
            Some(target.to_path_buf())
        }
        Err(_) => {
            // A concurrent healer republished `target`, or the publish failed. Drop
            // our tmp; restore the stale copy if `target` is now empty, else discard
            // it and adopt whatever published `target`.
            let _ = fs::remove_dir_all(&tmp);
            if moved_aside {
                if !target.exists() {
                    let _ = fs::rename(&stale, target);
                } else {
                    let _ = fs::remove_dir_all(&stale);
                }
            }
            target.is_dir().then(|| target.to_path_buf())
        }
    }
}

/// Stream-decompress the embedded zstd blob and unpack the tar into `dest`. The
/// tar entries are at the root (`preload.mjs`, `addons/…`, `node_modules/…`), so
/// they land directly in `dest`, reproducing the sidecar layout.
fn unpack_blob(dest: &Path) -> std::io::Result<()> {
    let decoder = ruzstd::decoding::StreamingDecoder::new(RUNTIME_BLOB)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut archive = tar::Archive::new(decoder);
    // The extracted runtime has no executables (the `.node` is dlopen'd, not
    // exec'd), so preserving the tar-recorded modes (read perms) is sufficient.
    archive.unpack(dest)
}

/// Remove stale siblings older than [`MAX_AGE`]: superseded `runtime-*` versions AND
/// leftover `.<key>.<pid>.<rand>.tmp` / `.stale.*` dirs orphaned by a crash mid-extract
/// or mid-self-heal. Best-effort, never throws, never touches the current dir. The age
/// gate is what makes evicting `.tmp`/`.stale.*` safe: a >30-day-old one is definitely
/// abandoned, never a concurrent extractor's in-progress dir (those are seconds old).
/// Runs only on the rare fresh-extract path.
fn gc_stale(base: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == *current {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let evictable =
            name.starts_with("runtime-") || name.starts_with(".stale.") || name.ends_with(".tmp");
        if !evictable {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|age| age > MAX_AGE)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// A short, collision-resistant suffix for the tmp dir name — dep-free
/// (SystemTime nanos XOR'd with a per-process atomic counter). It only needs to be
/// unique among this machine's concurrent extractors; the atomic guards two
/// same-process extractors and the nanos guard cross-process ones.
fn rand_suffix() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos
        ^ (COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Serializes the tests that mutate the shared `TEST_ENFORCE` static — libtest
    /// runs tests in parallel, so without this lock one test's `store` could land
    /// inside another's enforce/canary assertion window and flake it. Poison is
    /// recovered (a panicking test must not cascade-fail its serialized sibling).
    static ENFORCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Build a tiny zstd-19 tar blob matching the real embedded layout, so the
    /// unpack/rename/idempotence/race/GC logic can be exercised without the
    /// feature's build.rs output. Mirrors `unpack_blob`'s decode side.
    fn make_test_blob() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let preload = b"// preload\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(preload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "preload.mjs", &preload[..])
            .unwrap();

        let addon = b"\x7fELF-fake-addon";
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(addon.len() as u64);
        h2.set_mode(0o644);
        h2.set_cksum();
        builder
            .append_data(&mut h2, "addons/nub-native.node", &addon[..])
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        zstd::encode_all(&tar_bytes[..], 19).unwrap()
    }

    fn unpack_test_blob(blob: &[u8], dest: &Path) {
        let decoder = ruzstd::decoding::StreamingDecoder::new(blob).unwrap();
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(dest).unwrap();
    }

    #[test]
    fn blob_roundtrips_to_the_sidecar_layout() {
        let tmp = std::env::temp_dir().join(format!("nub-rtc-rt-{}", rand_suffix()));
        let _ = fs::remove_dir_all(&tmp);
        let blob = make_test_blob();
        unpack_test_blob(&blob, &tmp);

        let mut preload = String::new();
        fs::File::open(tmp.join("preload.mjs"))
            .unwrap()
            .read_to_string(&mut preload)
            .unwrap();
        assert_eq!(preload, "// preload\n");
        assert!(tmp.join("addons/nub-native.node").is_file());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn extract_then_atomic_rename_is_idempotent() {
        // First extract publishes the dir; a second pass over the same base + key
        // sees it present and reuses it byte-for-byte (no re-write).
        let base = std::env::temp_dir().join(format!("nub-rtc-idem-{}", rand_suffix()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let key = "runtime-test-deadbeef";
        let blob = make_test_blob();

        let publish = |base: &Path| -> PathBuf {
            let target = base.join(key);
            if target.is_dir() {
                return target;
            }
            let tmp = base.join(format!(".{key}.{}.tmp", rand_suffix()));
            fs::create_dir_all(&tmp).unwrap();
            unpack_test_blob(&blob, &tmp);
            match fs::rename(&tmp, &target) {
                Ok(()) => target,
                Err(_) => {
                    let _ = fs::remove_dir_all(&tmp);
                    target
                }
            }
        };

        let a = publish(&base);
        let mtime_a = fs::metadata(a.join("preload.mjs"))
            .unwrap()
            .modified()
            .unwrap();
        let b = publish(&base);
        let mtime_b = fs::metadata(b.join("preload.mjs"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(mtime_a, mtime_b, "second pass must not re-extract");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_only_base_create_probe_fails_cleanly() {
        // `ensure_safe_base`'s writability probe is `create_dir_all(base)`. Point it
        // at a path whose parent is a FILE (so create_dir_all can't succeed) and
        // confirm the probe fails rather than panicking — the production recovery to
        // the next candidate ($TMPDIR) rides on exactly this `is_err()`.
        let file = std::env::temp_dir().join(format!("nub-rtc-file-{}", rand_suffix()));
        fs::write(&file, b"x").unwrap();
        let unusable = file.join("subdir"); // parent is a file → create_dir_all errors
        assert!(ensure_safe_base(&unusable).is_none());
        fs::remove_file(&file).unwrap();
    }

    /// Fresh per-test base under `$TMPDIR`, pre-cleared.
    fn tmp_base(prefix: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("{prefix}-{}", rand_suffix()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    // ---- R2: verify-on-load against the REAL embedded blob + baked hashes --------

    #[test]
    fn embedded_blob_verifies_clean() {
        // The load-bearing zero-false-positive guarantee: a clean extraction of the
        // blob THIS binary embeds verifies against the hashes build.rs baked. If this
        // fails, verify-on-load would brick nub on this platform — so it runs wherever
        // `cargo test --features embed-runtime` does (the ci-gate embed-runtime job).
        let dir = tmp_base("nub-rtc-clean");
        fs::create_dir_all(&dir).unwrap();
        unpack_blob(&dir).unwrap();
        assert!(
            verify_entrypoints(&dir),
            "a clean extraction of the embedded blob must verify (zero false-positive)"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn baked_hashes_match_embedded_blob_entries() {
        // build.rs determinism: the digest the binary will COMPARE against must equal
        // the digest of the bytes it EXTRACTS. (build.rs hashes the staged file; tar
        // is byte-exact, so extracted == staged — this confirms it end-to-end.)
        let dir = tmp_base("nub-rtc-bake");
        fs::create_dir_all(&dir).unwrap();
        unpack_blob(&dir).unwrap();
        for (rel, expected) in VERIFIED_ENTRYPOINTS {
            let got = file_blake3_hex(&dir.join(rel)).unwrap();
            assert_eq!(
                got, expected,
                "baked digest for {rel} must equal blake3 of the extracted entry"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tampered_entrypoint_is_detected_and_self_healed() {
        // A WARM cache whose watch guard was swapped (planted / AV-corrupted) must be
        // detected and self-healed: re-extract the trusted in-binary blob over it,
        // restoring the verified bytes — never bricked, never silently loaded.
        let base = tmp_base("nub-rtc-heal");
        let target = base.join(CACHE_KEY);
        fs::create_dir_all(&target).unwrap();
        unpack_blob(&target).unwrap();
        assert!(
            verify_entrypoints(&target),
            "fresh real-blob extraction verifies"
        );

        fs::write(target.join("watch-env-guard.cjs"), b"malicious").unwrap();
        assert!(!verify_entrypoints(&target), "tamper must be detected");

        let healed = verify_or_heal(&base, &target, false).expect("self-heal returns the dir");
        assert_eq!(healed, target);
        assert!(
            verify_entrypoints(&target),
            "self-heal must restore the trusted bytes"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn persistent_mismatch_is_canary_by_default_and_refuses_under_enforce() {
        let _guard = ENFORCE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A dir whose entrypoints will NEVER match the baked hashes. `already_fresh`
        // skips the heal so we hit the terminal decision directly: canary proceeds
        // (never brick on a verify bug), enforce refuses (the flipped-on behavior).
        let base = tmp_base("nub-rtc-decide");
        let target = base.join(CACHE_KEY);
        fs::create_dir_all(target.join("addons")).unwrap();
        fs::write(target.join("preload.mjs"), b"wrong").unwrap();
        fs::write(target.join("preload.cjs"), b"wrong").unwrap();
        fs::write(target.join("addons/nub-native.node"), b"wrong").unwrap();
        assert!(!verify_entrypoints(&target));

        TEST_ENFORCE.store(1, Ordering::Relaxed); // canary
        assert_eq!(
            verify_or_heal(&base, &target, true).as_deref(),
            Some(target.as_path()),
            "canary mode proceeds with the dir"
        );

        TEST_ENFORCE.store(2, Ordering::Relaxed); // enforce
        assert!(
            verify_or_heal(&base, &target, true).is_none(),
            "enforce mode refuses on a persistent mismatch"
        );

        TEST_ENFORCE.store(0, Ordering::Relaxed); // reset for other tests
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn canary_never_returns_a_nonexistent_dir() {
        let _guard = ENFORCE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The canary contract is "always hand back a live dir or None". A failed
        // self-heal can leave `target` absent; the terminal canary branch must then
        // degrade to None rather than a ghost path the child `node` would brick on.
        // `target` is never created on disk, so verify_entrypoints fails and
        // already_fresh=true skips the heal — hitting the terminal branch with no dir.
        let base = tmp_base("nub-rtc-ghost");
        fs::create_dir_all(&base).unwrap();
        let target = base.join(CACHE_KEY);
        assert!(!target.exists(), "target must not exist on disk");

        TEST_ENFORCE.store(1, Ordering::Relaxed); // canary
        assert!(
            verify_or_heal(&base, &target, true).is_none(),
            "canary must degrade to None when target is a ghost dir"
        );

        TEST_ENFORCE.store(0, Ordering::Relaxed); // reset for other tests
        fs::remove_dir_all(&base).unwrap();
    }

    // ---- R1: per-user 0700 base + owner/perms/symlink validation -----------------

    #[cfg(unix)]
    #[test]
    fn is_safe_dir_accepts_owner_only_rejects_world_writable_and_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp_base("nub-rtc-safe");
        fs::create_dir_all(&base).unwrap();

        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_safe_dir(&base), "0700 owner-only dir is safe");

        fs::set_permissions(&base, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!is_safe_dir(&base), "group/world-writable dir is rejected");

        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.with_extension("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(
            !is_safe_dir(&link),
            "a symlinked base is rejected (no traversal)"
        );

        fs::remove_file(&link).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn extract_with_recovers_from_an_unsafe_base() {
        // R1 recovery, end-to-end: an unsafe (world-writable) candidate must be
        // SKIPPED, not bricked, and extraction must land in a fresh safe per-user
        // base — then verify clean.
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_base("nub-rtc-recover");
        fs::create_dir_all(&root).unwrap();

        let unsafe_base = root.join("unsafe");
        fs::create_dir_all(&unsafe_base).unwrap();
        fs::set_permissions(&unsafe_base, fs::Permissions::from_mode(0o777)).unwrap();
        let safe_base = root.join("safe"); // absent → ensure_safe_base creates it 0700

        let got = extract_with(&[unsafe_base.clone(), safe_base.clone()])
            .expect("recovers to the safe base");
        assert!(
            got.starts_with(&safe_base),
            "extraction must land in the safe base, got {got:?}"
        );
        assert!(verify_entrypoints(&got), "recovered extraction verifies");
        assert!(
            !unsafe_base.join(CACHE_KEY).exists(),
            "must never extract into an unsafe base"
        );

        // The created safe base is 0700.
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&safe_base).unwrap().mode() & 0o777,
            0o700,
            "the safe base is created owner-only"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    // Unix-only: the eviction assertion needs `filetime_set` to backdate the stale
    // dir, and that helper is a no-op off unix (see its `#[cfg(not(unix))]` arm), so
    // on other platforms the stale dir would keep its fresh mtime and survive.
    #[cfg(unix)]
    #[test]
    fn gc_evicts_stale_keeps_current_and_tmp() {
        let base = std::env::temp_dir().join(format!("nub-rtc-gc-{}", rand_suffix()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let current = base.join("runtime-cur");
        let stale = base.join("runtime-old");
        let tmp = base.join(".runtime-old.123.tmp"); // RECENT in-progress tmp
        let old_orphan = base.join(".stale.runtime-old.99.7"); // crashed-heal leftover
        let old_tmp = base.join(".runtime-old.42.9.tmp"); // crashed-extract leftover
        for d in [&current, &stale, &tmp, &old_orphan, &old_tmp] {
            fs::create_dir_all(d).unwrap();
        }
        // Backdate the stale version + the two abandoned orphans well past MAX_AGE.
        let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
        for d in [&stale, &old_orphan, &old_tmp] {
            filetime_set(d, old);
        }

        gc_stale(&base, &current);

        assert!(current.is_dir(), "current version must survive GC");
        assert!(!stale.is_dir(), "a >30d sibling must be evicted");
        assert!(
            tmp.is_dir(),
            "a RECENT in-progress .tmp dir must never be touched"
        );
        assert!(
            !old_orphan.is_dir(),
            "a >30d .stale.* orphan must be evicted"
        );
        assert!(!old_tmp.is_dir(), "a >30d .tmp orphan must be evicted");
        fs::remove_dir_all(&base).unwrap();
    }

    /// Set a dir's mtime via libc `utimes` (unix) — dep-free. On platforms where
    /// this isn't wired the GC age-test is skipped by leaving mtime as-is, which
    /// would make the eviction assertion fail loudly rather than silently pass, so
    /// keep it unix-gated.
    #[cfg(unix)]
    fn filetime_set(path: &Path, time: SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let secs = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let tv = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        unsafe {
            libc::utimes(c.as_ptr(), times.as_ptr());
        }
    }

    #[cfg(not(unix))]
    fn filetime_set(_path: &Path, _time: SystemTime) {}
}
