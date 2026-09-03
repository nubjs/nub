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
//!   Windows validates every existing namespace component for reparse points and
//!   unsafe owners/DACLs; missing components are created with a protected private
//!   DACL rather than inheriting access from a potentially shared parent.
//!
//! - **R2 — verify the loaded code against a baked-in hash (integrity backstop).**
//!   `build.rs` bakes the BLAKE3 digest of the five directly-loaded entrypoints
//!   (`preload.mjs`, `preload.cjs`, `watch-env-guard.cjs`,
//!   `compile-preamble.mjs`, and `addons/nub-native.node`) into the binary. On
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
//!   Compile has a separate, compile-only full-tree gate for its transitive support
//!   files: it performs one trusted re-extraction on mismatch, then always fails
//!   closed if the complete tree still differs. That gate is deliberately outside
//!   the normal load path and its [`EXTRACTED`] memoization.
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

use super::{discovery, runtime_tree};

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
const HASH_COMPILE_PREAMBLE: &str = env!("NUB_RUNTIME_HASH_COMPILE_PREAMBLE");
const HASH_ADDON: &str = env!("NUB_RUNTIME_HASH_ADDON");
const RUNTIME_TREE_BLAKE3: &str = env!("NUB_RUNTIME_TREE_BLAKE3");

/// The directly-loaded entrypoints and their baked digests. The native addon
/// (`dlopen`'d), preload scripts (`--require`d/`--import`ed), and compile
/// preamble (loaded from the extracted runtime) are the actual code-load surface; the vendored `node_modules` polyfills are intentionally OUT of the
/// per-load hash (R1's 0700 owner-only base already closes their planted-file
/// vector, and hashing the whole ~13 MB tree every run would be a real regression
/// for a fast script runner — the entrypoints keep the cost ~1-2 ms).
const VERIFIED_ENTRYPOINTS: [(&str, &str); 5] = [
    ("preload.mjs", HASH_PRELOAD_MJS),
    ("preload.cjs", HASH_PRELOAD_CJS),
    ("watch-env-guard.cjs", HASH_WATCH_ENV_GUARD),
    // `nub compile` loads this executable JS straight from the extracted runtime.
    ("compile-preamble.mjs", HASH_COMPILE_PREAMBLE),
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
pub(crate) fn ensure_runtime() -> Option<PathBuf> {
    EXTRACTED.get_or_init(extract_once).clone()
}

fn extract_once() -> Option<PathBuf> {
    let dir = extract_with(&base_candidates())?;
    // The addon is a Mach-O with no `codesign` step, so macOS refuses to
    // `dlopen` it once quarantined — and `transform-core.mjs` swallows that
    // load failure into a null handle it then calls unguarded, making every
    // TypeScript transpile a hard failure. The inodes are created in-process
    // (or adopted from a concurrent winner), so a nub carrying inherited
    // quarantine flags stamps them itself.
    //
    // Deliberately on the warm path too, not just a fresh extract:
    // `VERIFIED_ENTRYPOINTS` compares content hashes, and an xattr does not
    // change a file's bytes, so a dir poisoned by an older build verifies
    // clean forever and the self-heal re-extract never fires. Costs one
    // syscall per process — this sits behind the `OnceLock`.
    let addon = dir.join("addons").join("nub-native.node");
    if let Err(e) = crate::quarantine::clear(&addon) {
        tracing::debug!(
            "could not clear com.apple.quarantine on {}: {e}",
            addon.display()
        );
    }
    Some(dir)
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
        if is_safe_dir(&target) && target.join("preload.mjs").is_file() {
            if let Some(dir) = verify_or_heal(&safe_base, &target, false) {
                // The cache base and canonical target were both validated before
                // adoption. Reclaim only our aged crash leftovers now that warm use
                // has proved a complete current tree exists too.
                gc_stale(&safe_base, &target);
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
        let safe_base = match safe_base_or_reason(base) {
            Ok(safe_base) => safe_base,
            Err(reason) => {
                // Never decline a candidate silently. A Windows-only relocation hid
                // here for exactly that reason: the runtime cache moved to `$TMPDIR`
                // while every other nub cache kept honouring `XDG_CACHE_HOME`, and
                // nothing said so — the only symptom was a cache dir that never
                // appeared where it was configured to.
                //
                // Report the reason the walker gave rather than asserting one. The
                // old text said "unsafe owner or permissions" for every rejection,
                // including a read-only filesystem and a transient failure of the
                // Windows ACL query — so a log could not tell a real DACL problem
                // from a one-off, which is exactly the distinction anyone chasing an
                // intermittent relocation needs.
                eprintln!(
                    "nub: {} is not usable for the runtime cache ({reason}); \
                     trying the next location",
                    base.display()
                );
                continue;
            }
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

/// R1: resolve `base` to a safe canonical per-user dir, or `None` if it cannot
/// be made and validated (the caller then recovers by trying the next candidate).
///
/// - If absent: create every missing component `0700` atomically, so there is no
///   umask window where the base is briefly world-writable. This is also the
///   writability probe (a read-only FS fails here → `None` → next candidate).
/// - Validate ownership/perms (unix) even on a dir we just "created": creation tolerates
///   an attacker who pre-created the path (`EEXIST`), so the POST-create owner check is
///   what actually rejects a planted base. A failed validation returns `None` — we
///   neither use nor destroy a dir we don't own.
fn ensure_safe_base(base: &Path) -> Option<PathBuf> {
    safe_base_or_reason(base).ok()
}

/// [`ensure_safe_base`], keeping the error instead of collapsing it to `None`.
///
/// The two differ only in what survives, and the difference is diagnostic rather
/// than behavioural: a base is declined either way. It exists because the cold
/// path's message used to NAME a cause — "unsafe owner or permissions" — that
/// this function had not established. Every rejection reads the same in a log,
/// so a transient `GetNamedSecurityInfoW` failure, a read-only filesystem and a
/// genuinely world-writable directory are indistinguishable after the fact.
///
/// That cost a real investigation: an intermittent Windows CI failure where the
/// cache base was refused and the runtime silently relocated, with nothing in the
/// log to separate "the DACL is wrong" from "the ACL call failed this once". The
/// walkers already return a specific `io::Error` — `walk_windows_base` even
/// distinguishes a reparse point from an unsafe DACL — and all of it was being
/// discarded one frame above where it was needed.
#[cfg(unix)]
fn safe_base_or_reason(base: &Path) -> std::io::Result<PathBuf> {
    let base = canonical_unix_base_path(base)?;
    walk_unix_base(&base, true)?;
    fs::canonicalize(base)
}

#[cfg(not(any(unix, windows)))]
fn safe_base_or_reason(base: &Path) -> std::io::Result<PathBuf> {
    if !base.exists() {
        fs::create_dir_all(base)?;
    }
    if !is_safe_dir(base) {
        return Err(std::io::Error::other(
            "runtime cache path is not a directory we own",
        ));
    }
    fs::canonicalize(base)
}

#[cfg(windows)]
fn safe_base_or_reason(base: &Path) -> std::io::Result<PathBuf> {
    walk_windows_base(base, true)?;
    fs::canonicalize(base)
}

/// The same validated, owner-only base the runtime cache uses, for the smaller
/// caches that share the directory.
///
/// They previously reached it with a bare `create_dir_all`, so whichever of them
/// ran first decided the security posture of the whole tree — measured on Windows,
/// where node discovery's write left the root carrying its parent's inherited ACEs
/// and every later validated caller then refused that root. Sharing one seam means
/// the posture no longer depends on call order. `None` means the base is not ours
/// to write to; a cache is an optimization, so the caller skips it rather than
/// falling back to somewhere unvalidated.
pub(crate) fn ensure_safe_cache_dir(base: &Path) -> Option<PathBuf> {
    ensure_safe_base(base)
}

/// Resolve Unix's existing parent prefix before validating it component by
/// component. This permits OS-owned aliases such as macOS `/var` while ensuring
/// later validation sees only the real namespace that owns the cache path.
#[cfg(unix)]
fn canonical_unix_base_path(base: &Path) -> std::io::Result<PathBuf> {
    let absolute = if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir()?.join(base)
    };
    let leaf = absolute
        .file_name()
        .ok_or_else(|| std::io::Error::other("runtime cache base has no directory leaf"))?;
    let mut existing = absolute
        .parent()
        .ok_or_else(|| std::io::Error::other("runtime cache base has no parent"))?
        .to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| std::io::Error::other("runtime cache base escaped its root"))?
                    .to_os_string();
                missing.push(name);
                existing = existing
                    .parent()
                    .ok_or_else(|| std::io::Error::other("runtime cache base escaped its root"))?
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
    let mut canonical = fs::canonicalize(existing)?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    canonical.push(leaf);
    Ok(canonical)
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
            if is_safe_dir(&target) {
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

/// Is `path` a real directory owned by us under an ancestor chain no other
/// principal can replace. Windows validates the complete namespace and DACL
/// separately.
#[cfg(unix)]
fn is_safe_dir(path: &Path) -> bool {
    walk_unix_base(path, false).is_ok()
}

/// Validate or create every Unix namespace component using the same sticky-parent
/// policy as compiled-artifact caches. A shared non-sticky ancestor can rename a
/// private leaf, so leaf-only mode checks are insufficient.
#[cfg(unix)]
fn walk_unix_base(path: &Path, create_missing: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let euid = current_euid();
    let mut chain: Vec<_> = path.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    let mut parent: Option<fs::Metadata> = None;
    for (index, component) in chain.iter().enumerate() {
        let leaf = index + 1 == chain.len();
        let mut metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(parent) = &parent {
            unix_parent_protects_child(parent, metadata.as_ref(), euid)?;
        }
        if metadata.is_none() {
            if !create_missing {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(component) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            metadata = Some(fs::symlink_metadata(component)?);
            if let Some(parent) = &parent {
                unix_parent_protects_child(parent, metadata.as_ref(), euid)?;
            }
        }
        let metadata = metadata.expect("created or existing runtime-cache component");
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other("runtime cache path is a symlink"));
        }
        if !metadata.is_dir() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
        }
        if leaf && (metadata.uid() != euid || metadata.permissions().mode() & 0o022 != 0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "runtime cache base is not private",
            ));
        }
        parent = Some(metadata);
    }
    Ok(())
}

#[cfg(unix)]
fn unix_parent_protects_child(
    parent: &fs::Metadata,
    child: Option<&fs::Metadata>,
    euid: u32,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if parent.file_type().is_symlink() {
        return Err(std::io::Error::other("runtime cache ancestor is a symlink"));
    }
    if !parent.file_type().is_dir() {
        return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
    }
    if parent.uid() != euid && parent.uid() != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runtime cache ancestor is owned by another user",
        ));
    }
    let mode = parent.permissions().mode();
    if mode & 0o022 == 0 {
        return Ok(());
    }
    if mode & 0o1000 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runtime cache ancestor is writable and non-sticky",
        ));
    }
    if let Some(child) = child
        && child.uid() != euid
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runtime cache child under sticky ancestor is foreign-owned",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_safe_dir(path: &Path) -> bool {
    walk_windows_base(path, false).is_ok()
}

#[cfg(not(any(unix, windows)))]
fn is_safe_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| !meta.file_type().is_symlink() && meta.is_dir())
}

/// Validate every Windows namespace component with the same protected-DACL and
/// reparse-point policy as compiled-artifact caches. Creation occurs one component
/// at a time with the private DACL already attached, never through `create_dir_all`.
#[cfg(windows)]
fn walk_windows_base(path: &Path, create_missing: bool) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    let mut chain: Vec<_> = path.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    for (index, component) in chain.iter().enumerate() {
        let leaf = index + 1 == chain.len();
        let volume_root = index == 0;
        let metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_missing => {
                match crate::windows_security::create_private_directory(component) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                fs::symlink_metadata(component)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(error),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0400 != 0 {
            return Err(std::io::Error::other(
                "runtime cache path is a symlink or reparse point",
            ));
        }
        if !metadata.is_dir() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
        }
        if !crate::windows_security::directory_is_stable(component, leaf, volume_root)? {
            // The leaf is nub's own cache root, and it is NOT necessarily created
            // by this walk: node discovery writes `node-discovery.json` there
            // through a plain `create_dir_all` long before spawn reaches
            // `ensure_runtime`, so the root usually already exists carrying the
            // parent's inherited ACEs. On a volume whose root grants
            // `BUILTIN\Users:(CI)(AD)/(WD)` — the Windows default off the system
            // drive — that made the leaf permanently unusable and silently
            // relocated the runtime cache to `%TEMP%`, ignoring `XDG_CACHE_HOME`.
            // Harden the directory we own instead of declining it; ownership is
            // re-checked there, so a foreign-owned path is still refused. Only on
            // the creating walk — `is_safe_dir` validates an existing target and
            // must never mutate.
            let recovered = leaf
                && create_missing
                && crate::windows_security::harden_private_directory(component).is_ok()
                && crate::windows_security::directory_is_stable(component, leaf, volume_root)?;
            if !recovered {
                // Name the COMPONENT, not just the root the caller asked for. The
                // walk validates every ancestor, so "this path is unsafe" leaves a
                // reader unable to tell a runner- or installer-created parent from
                // the leaf nub makes itself — and only the leaf is recoverable, so
                // which one failed decides whether there is anything to fix. A CI
                // failure spent hours on that distinction with nothing in the log to
                // settle it.
                return Err(std::io::Error::other(format!(
                    "runtime cache path has an unsafe owner or DACL: {} ({})",
                    component.display(),
                    if leaf {
                        "the cache directory itself, which nub creates and hardens"
                    } else {
                        "an ancestor nub does not own, so its permissions are the \
                         system's to fix"
                    }
                )));
            }
        }
    }
    Ok(())
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

/// Strict complete-tree check for compile's extracted runtime reads.
fn verify_runtime_tree(dir: &Path) -> bool {
    runtime_tree::tree_blake3(dir)
        .map(|digest| digest.to_hex().as_str() == RUNTIME_TREE_BLAKE3)
        .unwrap_or(false)
}

/// Return the one cache location that a compile-time tree repair may trust. The
/// spelling is deliberately canonical and exact: a clean hash is not authority to
/// accept an arbitrary directory, a lexical alias, or a final redirect.
/// `fs::canonicalize` emits Windows verbatim (`\\?\`) spellings, but every
/// production caller arrives through `find_public_preload`, which strips that
/// prefix — and `Prefix::VerbatimDisk` never compares equal to `Prefix::Disk`.
/// Comparing the two spellings directly made the check below fail on every
/// Windows embedded-runtime build, so `nub compile` refused to run and blamed
/// the wrong subsystem. Pure over `windows` so both branches test on any host.
fn canonical_spelling_matches(canonical: &Path, given: &Path, windows: bool) -> bool {
    canonical == given
        || canonical
            .to_str()
            .is_some_and(|text| Path::new(&super::spawn::strip_verbatim(text, windows)) == given)
}

/// Is `dir` spelled as `base`, one separator, then [`CACHE_KEY`] — by BYTES?
/// `Path`'s `PartialEq` is component-wise and `Components` drops `.`, so
/// `<base>/./<key>` compares EQUAL to the canonical target and a lexical alias
/// would reach the tree repair this spelling exists to keep it out of. The
/// separator itself stays free: Windows accepts either, and the released spelling
/// arrives through `find_public_preload` rather than `Path::join`, so pinning one
/// would repeat the mismatch that already broke every Windows build once. Pure
/// over `windows` so both branches test on any host.
fn spelled_as_cache_key_below(dir: &Path, base: &Path, windows: bool) -> bool {
    let dir = dir.as_os_str().as_encoded_bytes();
    let base = base.as_os_str().as_encoded_bytes();
    let Some([separator, name @ ..]) = dir.strip_prefix(base) else {
        return false;
    };
    (*separator == b'/' || (windows && *separator == b'\\')) && name == CACHE_KEY.as_bytes()
}

fn canonical_runtime_target(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let base = dir.parent()?.to_path_buf();
    let canonical = fs::canonicalize(&base).ok()?;
    if !spelled_as_cache_key_below(dir, &base, cfg!(windows))
        || !canonical_spelling_matches(&canonical, &base, cfg!(windows))
        || !is_safe_dir(&base)
    {
        return None;
    }
    let metadata = fs::symlink_metadata(dir).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !is_safe_dir(dir) {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0400 != 0 {
            return None;
        }
    }
    Some((base, dir.to_path_buf()))
}

/// One bounded post-swap check for a concurrent healer that published the exact
/// canonical target after our own strict replacement lost its missing-target race.
/// A winner may be between rename-old and publish-new, so retry only a transient
/// missing target; any present-but-unsafe spelling fails immediately.
const RACING_WINNER_RETRIES: usize = 32;
fn clean_racing_winner(target: &Path, verify: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    for attempt in 0..=RACING_WINNER_RETRIES {
        match fs::symlink_metadata(target) {
            Ok(_) => {
                let (_, target) = canonical_runtime_target(target)?;
                return verify(&target).then_some(target);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if attempt == RACING_WINNER_RETRIES {
                    return None;
                }
                std::thread::yield_now();
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return None,
        }
    }
    unreachable!("the bounded race loop always returns")
}

/// Verify compile's complete extracted runtime tree, self-healing one mismatch
/// from the trusted embedded blob before failing closed.
///
/// This intentionally sits outside [`EXTRACTED`]: [`ensure_runtime`] memoizes the
/// normal launch-path entrypoint check, but compile reads transitive support files
/// and must detect corruption that happens before or after that memoization. The
/// full-tree hash is paid only by compile; ordinary launches retain the five-file
/// check and its canary policy.
pub(crate) fn verify_or_heal_embedded_runtime_tree(dir: &Path) -> bool {
    let Some((base, target)) = canonical_runtime_target(dir) else {
        return false;
    };
    if verify_runtime_tree(&target) {
        return true;
    }

    eprintln!(
        "nub: compiled executable runtime cache at {} did not match the embedded runtime; \
         re-extracting",
        target.display()
    );
    match swap_extract(&base, &target) {
        SwapExtract::Published(healed) if verify_runtime_tree(&healed) => return true,
        #[cfg(windows)]
        SwapExtract::InUse => {
            eprintln!(
                "nub: embedded runtime cache is in use by another nub/node process; close it and retry"
            );
            return false;
        }
        SwapExtract::Published(_) => {}
        SwapExtract::Failed => {
            if clean_racing_winner(&target, verify_runtime_tree).is_some() {
                return true;
            }
        }
    }

    eprintln!(
        "nub: compiled executable runtime integrity check failed at {} after \
         re-extraction; refusing to compile",
        target.display()
    );
    false
}

/// Re-hash the extracted entrypoints in `dir` against the baked digests. All five
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
    if !is_safe_dir(target) {
        return None;
    }
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
        match swap_extract(base, target) {
            SwapExtract::Published(healed) if verify_entrypoints(&healed) => return Some(healed),
            #[cfg(windows)]
            SwapExtract::InUse => {
                eprintln!(
                    "nub: embedded runtime cache is in use by another nub/node process; close it and retry"
                );
                return None;
            }
            SwapExtract::Published(_) => {}
            SwapExtract::Failed => {
                if let Some(winner) = clean_racing_winner(target, verify_entrypoints) {
                    return Some(winner);
                }
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
/// verify mismatch). The typed result distinguishes a live-DLL lock from ordinary
/// publication failures.
///
/// The stale dir is moved aside (atomic rename) then the fresh dir is renamed onto
/// the canonical name; a concurrent reader sees the complete old dir, a brief
/// absence, then the complete new dir — never a partial tree. The absence window is
/// same-uid-only (R1's 0700 owner-only base) and triggers at worst a redundant
/// cold-extract in a racing process, which is itself safe.
enum SwapExtract {
    Published(PathBuf),
    /// Windows cannot move a directory holding a loaded `nub-native.node`.
    /// The caller must fail closed without deleting or redirecting the target.
    #[cfg(windows)]
    InUse,
    Failed,
}

fn classify_target_rename_failure(error: &std::io::Error, target_remains: bool) -> SwapExtract {
    #[cfg(not(windows))]
    let _ = (error, target_remains);
    #[cfg(windows)]
    if target_remains && matches!(error.raw_os_error(), Some(5 | 32)) {
        return SwapExtract::InUse;
    }
    SwapExtract::Failed
}

fn swap_extract(base: &Path, target: &Path) -> SwapExtract {
    let tmp = unique_tmp(base);
    let _ = fs::remove_dir_all(&tmp);
    if fs::create_dir_all(&tmp).is_err() {
        return SwapExtract::Failed;
    }
    set_owner_only(&tmp);
    if let Err(e) = unpack_blob(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        eprintln!("nub: failed to re-inflate the embedded runtime during self-heal: {e}");
        return SwapExtract::Failed;
    }

    let stale = base.join(format!(
        ".stale.{CACHE_KEY}.{}.{}",
        std::process::id(),
        rand_suffix()
    ));
    if let Err(error) = fs::rename(target, &stale) {
        let _ = fs::remove_dir_all(&tmp);
        // A loaded Windows DLL blocks this first rename. The canonical target was
        // never moved, so retain it exactly and let the caller emit the actionable
        // fail-closed diagnostic rather than trying a destructive fallback.
        return classify_target_rename_failure(&error, target.exists());
    }

    match fs::rename(&tmp, target) {
        Ok(()) => {
            // Report a set-aside copy we could not remove rather than swallowing it.
            // On Windows this fails whenever a live process still has the old addon
            // mapped — deleting a mapped image is what the OS actually refuses, unlike
            // the rename above — so the tree strands until `gc_stale` collects it.
            // Discarding the error is what kept that invisible.
            if let Err(error) = fs::remove_dir_all(&stale) {
                eprintln!(
                    "nub: could not remove the superseded runtime at {}: {error}; \
                     it will be collected later",
                    stale.display()
                );
            }
            SwapExtract::Published(target.to_path_buf())
        }
        Err(_) => {
            let _ = fs::remove_dir_all(&tmp);
            // The target was moved only after the trusted replacement was fully
            // unpacked. Restore it rather than deleting/overwriting it or choosing
            // another runtime directory when publication cannot complete.
            if !target.exists() {
                let _ = fs::rename(&stale, target);
            } else {
                // A concurrent healer republished while we were unpacking, so the
                // set-aside copy is now unreachable garbage rather than the only
                // surviving tree. Drop it instead of stranding it until `gc_stale`.
                let _ = fs::remove_dir_all(&stale);
            }
            SwapExtract::Failed
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
/// leftover `.<key>.<pid>.<rand>.tmp` / `.stale.<key>.<pid>.<rand>` dirs orphaned by a
/// crash mid-extract or mid-self-heal. Best-effort, never throws, never touches the
/// current dir. The age gate keeps normal in-progress stages (which last seconds) out of
/// scope. Runs after fresh publication and safe warm adoption.
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
        if !name.starts_with("runtime-") && !is_runtime_orphan_name(&name) {
            continue;
        }
        // Do not follow an unexpected final symlink/reparse point while cleaning.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x0400 != 0 {
                continue;
            }
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

/// True only for the crash-orphan names this cache version creates. Keeping the
/// cache key and two decimal suffixes exact prevents GC from claiming unrelated
/// hidden directories that merely end in `.tmp` or start with `.stale.`.
fn is_runtime_orphan_name(name: &str) -> bool {
    let tmp = name
        .strip_prefix('.')
        .and_then(|name| name.strip_prefix(CACHE_KEY))
        .and_then(|name| name.strip_prefix('.'))
        .and_then(|name| name.strip_suffix(".tmp"));
    let stale = name
        .strip_prefix(".stale.")
        .and_then(|name| name.strip_prefix(CACHE_KEY))
        .and_then(|name| name.strip_prefix('.'));
    tmp.or(stale).is_some_and(has_decimal_pair)
}

fn has_decimal_pair(suffix: &str) -> bool {
    let mut parts = suffix.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(random), None)
            if !pid.is_empty()
                && !random.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && random.bytes().all(|byte| byte.is_ascii_digit())
    )
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

    /// The compile-time runtime-tree repair reaches this check through
    /// `find_public_preload`, which strips Windows' verbatim prefix, while
    /// `fs::canonicalize` emits it. Treating those two spellings as different
    /// directories made `nub compile` refuse every Windows embedded-runtime
    /// build and blame the integrity check. Both branches run on any host
    /// because the helper is pure over `windows`.
    #[test]
    fn a_verbatim_canonical_path_matches_its_stripped_spelling() {
        let stripped = Path::new(r"C:\Users\x\AppData\Local\nub");
        let verbatim = Path::new(r"\\?\C:\Users\x\AppData\Local\nub");
        assert!(canonical_spelling_matches(verbatim, stripped, true));

        let unc_stripped = Path::new(r"\\srv\share\nub");
        let unc_verbatim = Path::new(r"\\?\UNC\srv\share\nub");
        assert!(canonical_spelling_matches(unc_verbatim, unc_stripped, true));

        // A genuinely different directory is still rejected — stripping the
        // prefix must not become a wildcard.
        assert!(!canonical_spelling_matches(
            Path::new(r"\\?\C:\Users\x\AppData\Local\other"),
            stripped,
            true
        ));

        // Unix: identical spellings match, and no stripping is attempted.
        let unix = Path::new("/home/x/.cache/nub");
        assert!(canonical_spelling_matches(unix, unix, false));
        assert!(!canonical_spelling_matches(
            Path::new("/home/x/.cache/other"),
            unix,
            false
        ));
    }

    /// The alias rejection is byte-level, but the separator must stay free — a
    /// Windows spelling reaching this check comes from `find_public_preload`, not
    /// from `Path::join`, and pinning one separator is how the last comparison
    /// here refused every Windows build.
    #[test]
    fn the_cache_key_spelling_is_exact_but_separator_agnostic_on_windows() {
        // Every path is a literal: `Path::join` picks the HOST's separator, which
        // would silently stop exercising the branch under test on the other OS.
        let spelled = |dir: &str, base: &str, windows: bool| {
            spelled_as_cache_key_below(Path::new(dir), Path::new(base), windows)
        };
        let win_base = r"C:\Users\x\AppData\Local\nub";
        assert!(spelled(&format!(r"{win_base}\{CACHE_KEY}"), win_base, true));
        assert!(spelled(&format!("{win_base}/{CACHE_KEY}"), win_base, true));

        let base = "/home/x/.cache/nub";
        assert!(spelled(&format!("{base}/{CACHE_KEY}"), base, false));
        assert!(
            !spelled(&format!(r"{base}\{CACHE_KEY}"), base, false),
            "a backslash is an ordinary filename character on unix"
        );
        assert!(
            !spelled(&format!("{base}/./{CACHE_KEY}"), base, false),
            "a `.`-spelled lexical alias is not the canonical target"
        );
        assert!(
            !spelled(&format!("{base}/nested/{CACHE_KEY}"), base, false),
            "the target must sit DIRECTLY below its validated base"
        );
        assert!(
            !spelled(&format!("{base}/{CACHE_KEY}x"), base, false),
            "a longer name sharing the key's prefix is a different directory"
        );
        assert!(
            !spelled(&format!("{base}/{CACHE_KEY}"), "/home/x/.cache", false),
            "a base that is merely a path prefix is not this target's parent"
        );
    }

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

        // And the REASON survives, because the cold path prints it verbatim. A
        // rejection that reaches the log as a fixed string is why an intermittent
        // Windows relocation could not be told apart from a real DACL problem, so
        // the error being specific here is the whole point of keeping it.
        let reason = safe_base_or_reason(&unusable).expect_err("an unusable base is refused");
        assert!(
            !reason.to_string().is_empty(),
            "the refusal must carry a reason to print"
        );
        assert_ne!(
            reason.kind(),
            std::io::ErrorKind::Other,
            "a parent-is-a-file refusal must keep the OS error, not a generic one: {reason}"
        );

        fs::remove_file(&file).unwrap();
    }

    /// Fresh per-test base under `$TMPDIR`, pre-cleared.
    fn tmp_base(prefix: &str) -> PathBuf {
        let p = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("{prefix}-{}", rand_suffix()));
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
    fn compile_runtime_tree_heals_transitive_changes() {
        let base = ensure_safe_base(&tmp_base("nub-rtc-tree")).unwrap();
        let target = base.join(CACHE_KEY);
        fs::create_dir_all(&target).unwrap();
        set_owner_only(&base);
        unpack_blob(&target).unwrap();
        assert!(verify_or_heal_embedded_runtime_tree(&target));

        let support = target.join("worker-blob-url.cjs");
        let expected = fs::read(&support).unwrap();

        fs::write(&support, b"tampered").unwrap();
        assert!(
            verify_or_heal_embedded_runtime_tree(&target),
            "compile must heal a modified transitive runtime dependency"
        );
        assert_eq!(
            fs::read(&support).unwrap(),
            expected,
            "self-heal must restore the exact embedded support-file bytes"
        );

        fs::remove_file(&support).unwrap();
        assert!(
            verify_or_heal_embedded_runtime_tree(&target),
            "compile must heal a removed transitive runtime dependency"
        );
        assert_eq!(fs::read(&support).unwrap(), expected);

        let extra = target.join("unexpected-runtime-file.cjs");
        fs::write(&extra, b"unexpected").unwrap();
        assert!(
            verify_or_heal_embedded_runtime_tree(&target),
            "compile must heal an unexpected runtime-tree addition"
        );
        assert!(!extra.exists(), "self-heal must remove unexpected files");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn compile_runtime_tree_accepts_only_the_canonical_cache_target() {
        let base = ensure_safe_base(&tmp_base("nub-rtc-authority")).unwrap();
        let target = base.join(CACHE_KEY);
        fs::create_dir_all(&target).unwrap();
        set_owner_only(&base);
        unpack_blob(&target).unwrap();
        assert!(
            verify_or_heal_embedded_runtime_tree(&target),
            "the canonical cache target is accepted"
        );

        let arbitrary = tmp_base("nub-rtc-arbitrary");
        fs::create_dir_all(&arbitrary).unwrap();
        unpack_blob(&arbitrary).unwrap();
        assert!(
            verify_runtime_tree(&arbitrary),
            "control: arbitrary tree is clean"
        );
        assert!(
            !verify_or_heal_embedded_runtime_tree(&arbitrary),
            "a clean tree outside the exact cache-key target is not authority"
        );

        // A real alias reaches this check through `find_public_preload`, which
        // hands over a verbatim-STRIPPED path — and that stripping is what makes
        // the fixture constructible at all: `PathBuf::push` deletes `.` from a
        // VERBATIM (`\\?\`) path, because Windows does not resolve it there, so
        // `base.join(".")` on the canonicalized base returns the canonical target
        // itself and this block would assert the exact opposite of the assertion
        // above. Stripping is a no-op off Windows, so unix keeps testing what it
        // always did.
        let alias_base = PathBuf::from(crate::node::spawn::strip_verbatim(
            base.to_str().expect("cache base is UTF-8"),
            cfg!(windows),
        ));
        let lexical = alias_base.join(".").join(CACHE_KEY);
        // Byte-wise on purpose: `Path`'s `PartialEq` drops `.`, so a `Path`
        // comparison here would pass against the very normalization it guards.
        assert_ne!(
            lexical.as_os_str(),
            alias_base.join(CACHE_KEY).as_os_str(),
            "the `.` must survive into the fixture, or this tests nothing"
        );
        assert!(
            verify_runtime_tree(&lexical),
            "control: lexical alias hashes clean"
        );
        assert!(
            !verify_or_heal_embedded_runtime_tree(&lexical),
            "a lexical alias is rejected before a clean hash can be accepted"
        );
        fs::remove_dir_all(&arbitrary).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compile_runtime_tree_rejects_a_noncanonical_base_alias() {
        let parent = tmp_base("nub-rtc-base-link");
        let actual_base = parent.join("actual-base");
        let target = actual_base.join(CACHE_KEY);
        fs::create_dir_all(&target).unwrap();
        set_owner_only(&actual_base);
        unpack_blob(&target).unwrap();
        let alias = parent.join("base-alias");
        std::os::unix::fs::symlink(&actual_base, &alias).unwrap();
        let noncanonical = alias.join(CACHE_KEY);
        assert!(
            verify_runtime_tree(&noncanonical),
            "control: noncanonical alias hashes clean"
        );
        assert!(
            !verify_or_heal_embedded_runtime_tree(&noncanonical),
            "the cache target must be spelled below its validated canonical base"
        );
        fs::remove_file(&alias).unwrap();
        fs::remove_dir_all(&parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compile_runtime_tree_rejects_a_final_symlink_even_when_its_tree_is_clean() {
        let base = ensure_safe_base(&tmp_base("nub-rtc-final-link")).unwrap();
        let target = base.join(CACHE_KEY);
        let actual = base.join("trusted-tree");
        fs::create_dir_all(&actual).unwrap();
        set_owner_only(&base);
        unpack_blob(&actual).unwrap();
        std::os::unix::fs::symlink(&actual, &target).unwrap();
        // The control has to hash the real directory: `runtime_tree` refuses a
        // symlinked ROOT outright rather than following it, so the link spelling
        // cannot be used to establish that the tree behind it is clean.
        assert!(
            verify_runtime_tree(&actual),
            "control: the tree behind the final link is clean"
        );
        assert!(
            !verify_or_heal_embedded_runtime_tree(&target),
            "the final runtime-cache target must be a real directory, not a link"
        );
        fs::remove_file(&target).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_cache_base_rejects_shared_dacl_and_creates_private_dacl() {
        let shared = tmp_base("nub-rtc-shared-dacl");
        crate::windows_security::create_shared_directory(&shared).unwrap();
        assert!(
            !is_safe_dir(&shared),
            "an Everyone-writable runtime cache base must be rejected"
        );

        let requested = tmp_base("nub-rtc-private-dacl");
        let created = ensure_safe_base(&requested).expect("private cache base is created");
        assert!(is_safe_dir(&created), "created cache base must validate");
        assert!(
            crate::windows_security::directory_is_stable(&created, true, false).unwrap(),
            "created runtime cache base must receive a protected private DACL"
        );
        fs::remove_dir_all(&shared).unwrap();
        fs::remove_dir_all(&created).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_cache_base_rejects_a_junction() {
        use std::os::windows::fs::symlink_dir;
        use std::process::Command;

        let root = ensure_safe_base(&tmp_base("nub-rtc-base-reparse")).unwrap();
        let target = root.join("target");
        let junction = root.join("base-junction");
        fs::create_dir(&target).unwrap();
        let created = Command::new("cmd")
            .arg("/C")
            .arg(format!(
                "mklink /J \"{}\" \"{}\"",
                junction.display(),
                target.display()
            ))
            .output()
            .unwrap();
        if !created.status.success() {
            if let Err(error) = symlink_dir(&target, &junction) {
                if error.raw_os_error() == Some(1314) {
                    eprintln!(
                        "skipping Windows runtime-cache base reparse test: junction and symlink creation unavailable"
                    );
                    fs::remove_dir_all(&root).unwrap();
                    return;
                }
                panic!("creating Windows runtime-cache base reparse point: {error}");
            }
        }
        assert!(
            ensure_safe_base(&junction).is_none(),
            "a junction must never be adopted as the runtime-cache base"
        );
        fs::remove_dir(&junction).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn compile_runtime_tree_rejects_a_final_reparse_target_even_when_clean() {
        use std::process::Command;

        let base = ensure_safe_base(&tmp_base("nub-rtc-final-reparse")).unwrap();
        let target = base.join(CACHE_KEY);
        let actual = base.join("trusted-tree");
        fs::create_dir_all(&actual).unwrap();
        unpack_blob(&actual).unwrap();
        let junction = Command::new("cmd")
            .arg("/C")
            .arg(format!(
                "mklink /J \"{}\" \"{}\"",
                target.display(),
                actual.display()
            ))
            .output()
            .unwrap();
        // `mklink` rejects the `\\?\` verbatim spelling `tmp_base` produces, so on
        // CI this always failed and the skip swallowed the whole test. A directory
        // symlink carries the same FILE_ATTRIBUTE_REPARSE_POINT the check under
        // test reads, so fall back to it and skip only when the runner genuinely
        // withholds the privilege — matching the sibling base-reparse test.
        if !junction.status.success()
            && let Err(error) = std::os::windows::fs::symlink_dir(&actual, &target)
        {
            if error.raw_os_error() == Some(1314) {
                eprintln!(
                    "skipping Windows final-reparse test: junction and symlink creation unavailable"
                );
                fs::remove_dir_all(&base).unwrap();
                return;
            }
            panic!("creating Windows final-reparse point: {error}");
        }
        // Hash the real directory, not the link — `runtime_tree` refuses a
        // reparse-point ROOT rather than following it (`is_symlink` covers both a
        // symlink and a junction on Windows), so the link spelling cannot be used
        // to establish that the tree behind it is clean. Same reasoning, and the
        // same shape, as the unix sibling above.
        assert!(
            verify_runtime_tree(&actual),
            "control: the tree behind the final reparse point is clean"
        );
        assert!(
            !verify_or_heal_embedded_runtime_tree(&target),
            "the final runtime-cache target must not be a reparse point"
        );
        fs::remove_dir(&target).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_loaded_runtime_rename_is_classified_as_in_use_only_while_target_remains() {
        assert!(matches!(
            classify_target_rename_failure(&std::io::Error::from_raw_os_error(32), true),
            SwapExtract::InUse
        ));
        assert!(matches!(
            classify_target_rename_failure(&std::io::Error::from_raw_os_error(5), true),
            SwapExtract::InUse
        ));
        assert!(matches!(
            classify_target_rename_failure(&std::io::Error::from_raw_os_error(32), false),
            SwapExtract::Failed
        ));
        assert!(matches!(
            classify_target_rename_failure(&std::io::Error::from_raw_os_error(3), true),
            SwapExtract::Failed
        ));
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
        // A WARM cache whose compile preamble was swapped (planted / AV-corrupted) must be
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

        fs::write(target.join("compile-preamble.mjs"), b"malicious").unwrap();
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

    #[test]
    fn clean_racing_winner_accepts_a_delayed_clean_publisher_once() {
        let base = ensure_safe_base(&tmp_base("nub-rtc-racing-winner")).unwrap();
        let target = base.join(CACHE_KEY);
        let publisher_target = target.clone();
        // Stage OUTSIDE the raced window, as production does: `swap_extract` inflates
        // its tmp dir before it moves the old target aside, so what the bounded retry
        // has to cover is two renames — not an inflate, which alone outlasts the bound
        // in a debug build and would make this a timing coin-flip.
        let publisher_staging = base.join("publisher-staging");
        fs::create_dir_all(&publisher_staging).unwrap();
        unpack_blob(&publisher_staging).unwrap();
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            fs::rename(&publisher_staging, &publisher_target).unwrap();
        });
        let checks = std::sync::atomic::AtomicUsize::new(0);
        assert_eq!(
            clean_racing_winner(&target, |path| {
                checks.fetch_add(1, Ordering::SeqCst);
                verify_entrypoints(path)
            })
            .as_deref(),
            Some(target.as_path()),
            "a clean concurrent publisher wins the bounded missing-target race"
        );
        publisher.join().unwrap();
        assert_eq!(
            checks.load(Ordering::SeqCst),
            1,
            "the winner is re-hashed once"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn clean_racing_winner_bounds_an_absent_target_without_hashing() {
        let base = ensure_safe_base(&tmp_base("nub-rtc-racing-absent")).unwrap();
        let target = base.join(CACHE_KEY);
        assert!(
            clean_racing_winner(&target, |_| panic!("an absent target must not be hashed"))
                .is_none(),
            "an absent target exhausts the short bounded retry without acceptance"
        );
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
    fn runtime_cache_rejects_a_private_base_under_a_non_sticky_shared_ancestor() {
        use std::os::unix::fs::PermissionsExt;

        let root = ensure_safe_base(&tmp_base("nub-rtc-shared-ancestor")).unwrap();
        let parent = root.join("shared");
        let base = parent.join("runtime");
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            !is_safe_dir(&base),
            "a non-sticky shared ancestor can substitute a 0700 cache base"
        );
        assert!(
            ensure_safe_base(&base).is_none(),
            "a pre-existing base below a non-sticky shared ancestor is not adopted"
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_cache_creates_a_private_base_below_a_sticky_shared_parent() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = ensure_safe_base(&tmp_base("nub-rtc-sticky-ancestor")).unwrap();
        let parent = root.join("tmp");
        let base = parent.join(format!("nub-{}", current_euid()));
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).unwrap();
        let created =
            ensure_safe_base(&base).expect("sticky parent permits an atomic owner cache base");
        assert_eq!(
            fs::metadata(&created).unwrap().mode() & 0o777,
            0o700,
            "the cache base under a sticky parent is owner-only"
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&root).unwrap();
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
            got.starts_with(fs::canonicalize(&safe_base).unwrap()),
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

        let current = base.join(CACHE_KEY);
        let stale = base.join("runtime-old");
        let tmp = base.join(format!(".{CACHE_KEY}.123.456.tmp")); // RECENT in-progress tmp
        let old_orphan = base.join(format!(".stale.{CACHE_KEY}.99.7")); // crashed-heal leftover
        let old_tmp = base.join(format!(".{CACHE_KEY}.42.9.tmp")); // crashed-extract leftover
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

    #[cfg(unix)]
    #[test]
    fn warm_adoption_reclaims_only_aged_recognized_crash_orphans() {
        // A crashes with both kinds of owned stage left behind; B has already
        // published the complete canonical tree; C must adopt it without a cold
        // extraction and reclaim only aged names that our cache format recognizes.
        let base = ensure_safe_base(&tmp_base("nub-rtc-warm-gc")).unwrap();
        let current = base.join(CACHE_KEY);
        fs::create_dir_all(&current).unwrap();
        unpack_blob(&current).unwrap();
        assert!(
            verify_entrypoints(&current),
            "B's canonical tree is complete"
        );

        let old_tmp = base.join(format!(".{CACHE_KEY}.11.22.tmp"));
        let old_stale = base.join(format!(".stale.{CACHE_KEY}.33.44"));
        let recent_tmp = base.join(format!(".{CACHE_KEY}.55.66.tmp"));
        let unrelated_tmp = base.join(".other-cache.11.22.tmp");
        let unrelated_stale = base.join(".stale.other-cache.33.44");
        for dir in [
            &old_tmp,
            &old_stale,
            &recent_tmp,
            &unrelated_tmp,
            &unrelated_stale,
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
        for dir in [&old_tmp, &old_stale, &unrelated_tmp, &unrelated_stale] {
            filetime_set(dir, old);
        }

        let adopted = extract_with(std::slice::from_ref(&base));
        assert_eq!(
            adopted.as_deref(),
            Some(current.as_path()),
            "C warm-adopts B's tree"
        );
        assert!(
            current.is_dir(),
            "the active canonical tree is never collected"
        );
        assert!(
            !old_tmp.exists(),
            "aged recognized extract stage is collected"
        );
        assert!(
            !old_stale.exists(),
            "aged recognized self-heal stage is collected"
        );
        assert!(
            recent_tmp.is_dir(),
            "recent live extraction stage is retained"
        );
        assert!(
            unrelated_tmp.is_dir(),
            "unrelated .tmp directory is retained"
        );
        assert!(
            unrelated_stale.is_dir(),
            "unrelated .stale directory is retained"
        );
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
