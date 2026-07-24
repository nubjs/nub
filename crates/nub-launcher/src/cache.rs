//! Where a compiled binary extracts its Node + app payload — resolved by WRITE
//! PROBE, never by the existence of an env var.
//!
//! The launcher runs in environments `nub run` never sees: AWS Lambda (`HOME` is
//! set, everything but `/tmp` is read-only), a Kubernetes pod with
//! `readOnlyRootFilesystem`, a distroless container. `nub_core`'s
//! `discovery::cache_dir()` answers "where would the cache live", which is the
//! right question for the CLI and the wrong one here: it returns `Some` whenever
//! `$HOME` is set, so a fallback chained off its `None` can never fire on exactly
//! the boxes that need one. This module answers "where can this process actually
//! write and exec" by trying each candidate for real.
//!
//! Security posture on the temp candidate: `vercel/pkg` extracted executable
//! content to a hardcoded, world-shared `/tmp/pkg/*`, which let a local attacker
//! pre-seed the path and get their code exec'd by someone else's binary
//! (CVE-2024-24828). The temp candidate here is therefore per-uid, mode 0700,
//! never a symlink, and rejected outright if it already exists owned by anyone
//! else — the launcher falls through rather than exec'ing out of a directory a
//! second principal can write. Extracted payloads stay keyed by content hash on
//! top of that, so a same-uid path substitution changes the key rather than the
//! contents behind it.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Why one candidate was passed over. Carried into the all-candidates-failed
/// error so the message can name the actual obstacle per path.
#[derive(Debug)]
enum Reject {
    /// The source env var is unset or empty.
    Unset,
    /// The filesystem is mounted read-only.
    ReadOnly,
    /// Mounted `noexec`: writable, but nothing under it can be exec'd.
    NoExec,
    /// Exists, owned by another uid.
    ForeignOwner,
    /// Exists with write bits for a principal other than the owner.
    Shared,
    /// Exists as a symlink where a private directory is required.
    Symlinked,
    /// Anything else — permissions, ENOSPC, ENOTDIR.
    Unusable(io::Error),
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reject::Unset => write!(f, "not set"),
            Reject::ReadOnly => write!(f, "read-only filesystem"),
            Reject::NoExec => write!(f, "mounted noexec"),
            Reject::ForeignOwner => write!(f, "owned by another user"),
            Reject::Shared => write!(f, "writable by other users"),
            Reject::Symlinked => write!(f, "is a symlink"),
            Reject::Unusable(e) => write!(f, "{e}"),
        }
    }
}

struct Candidate {
    /// How the path is spelled in the error report — the env var, not the value.
    label: &'static str,
    path: Option<PathBuf>,
    /// A world-writable parent (the temp dir) means this directory must be ours
    /// alone: created 0700, never a symlink, no group/other write bits.
    private: bool,
}

/// Every candidate that was tried and why each failed.
pub struct CacheError {
    attempts: Vec<(&'static str, Option<PathBuf>, Reject)>,
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "no writable, executable cache directory was found.")?;
        writeln!(f, "  Tried:")?;
        for (label, path, why) in &self.attempts {
            match path {
                Some(p) => writeln!(f, "    {label} ({}) — {why}", p.display())?,
                None => writeln!(f, "    {label} — {why}")?,
            }
        }
        write!(
            f,
            "  Any one of these fixes it:\n\
             \x20   - point NUB_COMPILE_CACHE_DIR at a writable directory that permits exec\n\
             \x20   - mount a writable exec volume (Docker: --tmpfs /tmp:exec; \
             Kubernetes: an emptyDir volume)\n\
             \x20   - run this binary once at image-build time with NUB_COMPILE_CACHE_DIR set \
             to a path baked into the image, so the runtime hit is warm and writes nothing\n\
             \x20   - build with --smol against a Node already installed on the box, \
             which extracts no runtime"
        )
    }
}

impl fmt::Debug for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for CacheError {}

/// The first candidate this process can actually create, write into, and exec
/// from. Order: `NUB_COMPILE_CACHE_DIR` → `$XDG_CACHE_HOME/nub` →
/// `$HOME/.cache/nub` → a per-uid directory under the temp dir.
pub fn resolve() -> Result<PathBuf, CacheError> {
    resolve_from(candidates())
}

/// The chain, over an explicit candidate list so it can be driven in a test
/// without mutating process-global environment variables.
fn resolve_from(candidates: Vec<Candidate>) -> Result<PathBuf, CacheError> {
    let mut attempts = Vec::new();
    for cand in candidates {
        let Some(path) = cand.path else {
            attempts.push((cand.label, None, Reject::Unset));
            continue;
        };
        match probe(&path, cand.private) {
            Ok(()) => return Ok(path),
            Err(why) => attempts.push((cand.label, Some(path), why)),
        }
    }
    Err(CacheError { attempts })
}

fn candidates() -> Vec<Candidate> {
    vec![
        // An explicit override is used verbatim — no `nub` subdirectory appended.
        // It is the documented escape hatch for read-only deploys and for warming
        // the cache into an image layer at build time.
        Candidate {
            label: "NUB_COMPILE_CACHE_DIR",
            path: non_empty_var("NUB_COMPILE_CACHE_DIR").map(PathBuf::from),
            private: false,
        },
        Candidate {
            label: "XDG_CACHE_HOME",
            path: non_empty_var("XDG_CACHE_HOME").map(|v| PathBuf::from(v).join("nub")),
            private: false,
        },
        Candidate {
            label: "HOME",
            path: home_cache(),
            private: false,
        },
        Candidate {
            label: "TMPDIR",
            path: Some(std::env::temp_dir().join(temp_leaf())),
            private: true,
        },
    ]
}

/// The same directory `nub run` and the package manager use, so a compiled
/// binary shares one extracted-Node cache with the rest of the toolchain.
#[cfg(unix)]
fn home_cache() -> Option<PathBuf> {
    non_empty_var("HOME").map(|h| PathBuf::from(h).join(".cache").join("nub"))
}

/// Windows has no single spelling of this: a normal profile keeps `~/.cache/nub`
/// while a service account falls back to `%LOCALAPPDATA%\nub`. Defer to nub-core
/// so a compiled binary lands on the same cache the CLI already populated.
#[cfg(not(unix))]
fn home_cache() -> Option<PathBuf> {
    nub_core::node::discovery::cache_dir()
}

/// The temp leaf is per-uid so two users on one box never share an extraction
/// directory. Windows temp is already per-user (`%LOCALAPPDATA%\Temp`).
#[cfg(unix)]
fn temp_leaf() -> String {
    format!("nub-{}", unsafe { libc::geteuid() })
}
#[cfg(not(unix))]
fn temp_leaf() -> String {
    "nub".to_string()
}

fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Create the directory if needed, verify it is ours, write a probe file, and
/// confirm the mount permits exec. Every step is an action, not an inspection —
/// the whole point is that `Some(path)` proves nothing.
fn probe(path: &Path, private: bool) -> Result<(), Reject> {
    inspect_existing(path, private)?;
    create(path, private).map_err(classify)?;
    write_probe(path).map_err(classify)?;
    if mount_is_noexec(path) {
        return Err(Reject::NoExec);
    }
    Ok(())
}

/// Ownership + mode gate on a directory that already exists.
///
/// Foreign ownership disqualifies every candidate: the launcher exec's a binary
/// out of this tree, so a directory another uid controls is a code-execution
/// handoff. The write-bit rule is deliberately split. Other-write (`o+w`) is
/// never legitimate for an exec source, so it disqualifies everywhere.
/// Group-write is refused only for the temp candidate: under a user-private-group
/// umask of 002 (the RHEL/Fedora default) `~/.cache/nub` is legitimately 0775
/// with the group being the user alone, and rejecting that would push those hosts
/// off the shared cache for no security gain — whereas in a world-writable temp
/// dir the launcher creates its own directory 0700, so a group bit there means
/// someone else made it.
#[cfg(unix)]
fn inspect_existing(path: &Path, private: bool) -> Result<(), Reject> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let md = match fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(classify(e)),
    };
    // A symlink at the private path redirects the extraction somewhere the
    // ownership check below would then evaluate against the wrong inode.
    if private && md.file_type().is_symlink() {
        return Err(Reject::Symlinked);
    }
    let md = if md.file_type().is_symlink() {
        match fs::metadata(path) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(classify(e)),
        }
    } else {
        md
    };
    if md.uid() != unsafe { libc::geteuid() } {
        return Err(Reject::ForeignOwner);
    }
    let mode = md.permissions().mode();
    let forbidden = if private { 0o077 } else { 0o002 };
    if mode & forbidden != 0 {
        return Err(Reject::Shared);
    }
    Ok(())
}

/// Windows has no uid and no mode bits to check. The temp directory is already
/// per-user there (`%LOCALAPPDATA%\Temp`), which is the property the unix branch
/// has to construct, so the write probe alone carries this platform.
#[cfg(not(unix))]
fn inspect_existing(_path: &Path, _private: bool) -> Result<(), Reject> {
    Ok(())
}

#[cfg(unix)]
fn create(path: &Path, private: bool) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    if private {
        b.mode(0o700);
    }
    b.create(path)
}

#[cfg(not(unix))]
fn create(path: &Path, _private: bool) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// `create_dir_all` succeeding on an existing directory says nothing about
/// writability, so write a real file.
fn write_probe(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".nub-write-probe.{}", std::process::id()));
    let result = fs::write(&probe, b"");
    let _ = fs::remove_file(&probe);
    result
}

fn classify(e: io::Error) -> Reject {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::EROFS) {
        return Reject::ReadOnly;
    }
    Reject::Unusable(e)
}

/// Whether the mount holding `dir` forbids exec.
///
/// This is the distinction the failure mode hinges on: a `noexec` mount accepts
/// every byte written and then fails the exec with `EACCES`, which reads as a
/// permissions bug rather than a mount-flag one. Asking the mount up front turns
/// it into a candidate rejection instead of a 100 MB extraction that dead-ends.
/// Docker's `--tmpfs /tmp` is `noexec` by default, so this is the common case,
/// not an exotic one.
#[cfg(target_os = "linux")]
fn mount_is_noexec(dir: &Path) -> bool {
    let Some(st) = statvfs(dir) else { return false };
    st.f_flag & libc::ST_NOEXEC != 0
}

#[cfg(target_os = "linux")]
fn statvfs(dir: &Path) -> Option<libc::statvfs> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then_some(st)
}

#[cfg(target_os = "macos")]
fn mount_is_noexec(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    st.f_flags & libc::MNT_NOEXEC as u32 != 0
}

/// No portable mount-flag query elsewhere; the exec-time classifier in `main.rs`
/// is the backstop on those hosts.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mount_is_noexec(_dir: &Path) -> bool {
    false
}

/// Whether an exec failure came from a `noexec` mount rather than a bad path or
/// a missing exec bit. The launcher chmods the extracted Node 0755 and has just
/// written it, so `EACCES` at exec time leaves the mount flag as the explanation.
pub fn exec_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

/// The remedies for a `noexec` cache directory, appended to the exec failure.
pub fn noexec_remedy(dir: &Path) -> String {
    format!(
        "the cache directory {} is on a filesystem mounted noexec, so the Node \
         runtime extracted there cannot be executed.\n\
         \x20 Any one of these fixes it:\n\
         \x20   - point NUB_COMPILE_CACHE_DIR at a directory that permits exec\n\
         \x20   - mount the volume with exec (Docker: --tmpfs /tmp:exec)\n\
         \x20   - build with --smol against a Node already installed on the box, \
         which extracts no runtime",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nub-cachetest-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn probe_creates_and_accepts_a_writable_directory() {
        let root = scratch("writable");
        let target = root.join("cache");
        assert!(probe(&target, false).is_ok());
        assert!(target.is_dir());
        // The probe file must not survive the probe.
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn private_probe_creates_the_directory_0700() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("private-mode");
        let target = root.join("nub-1234");
        probe(&target, true).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "private cache dir must be 0700, got {mode:o}");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn private_probe_refuses_a_group_or_world_writable_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("shared-perms");

        for mode in [0o777, 0o770, 0o707] {
            let target = root.join(format!("d{mode:o}"));
            fs::create_dir_all(&target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
            let err = probe(&target, true).unwrap_err();
            assert!(
                matches!(err, Reject::Shared),
                "mode {mode:o} must be refused as shared, got {err}"
            );
        }

        // The non-private candidates tolerate group-write (a user-private-group
        // umask of 002 makes it routine) but never other-write.
        let group = root.join("group-writable");
        fs::create_dir_all(&group).unwrap();
        fs::set_permissions(&group, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(probe(&group, false).is_ok());

        let other = root.join("other-writable");
        fs::create_dir_all(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o757)).unwrap();
        assert!(matches!(probe(&other, false), Err(Reject::Shared)));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn private_probe_refuses_a_symlinked_directory() {
        let root = scratch("symlink");
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(probe(&link, true), Err(Reject::Symlinked)));
        // A non-private candidate may legitimately be a symlink (~/.cache often is).
        assert!(probe(&link, false).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn probe_rejects_an_unwritable_directory() {
        use std::os::unix::fs::PermissionsExt;
        // Running as root defeats mode-based denial, so this assertion is only
        // meaningful for an ordinary user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = scratch("readonly-dir");
        let target = root.join("locked");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(matches!(probe(&target, false), Err(Reject::Unusable(_))));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// The P0 this module exists for: an unwritable candidate must be SKIPPED,
    /// not accepted because its path could be spelled. The old chain tested
    /// `Option`-ness, so a read-only `$HOME` won and the fallback was dead code.
    #[cfg(unix)]
    #[test]
    fn an_unwritable_candidate_falls_through_to_the_next() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = scratch("fallthrough");
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let usable = root.join("usable");

        let chosen = resolve_from(vec![
            Candidate {
                label: "NUB_COMPILE_CACHE_DIR",
                path: None,
                private: false,
            },
            Candidate {
                label: "HOME",
                path: Some(locked.clone()),
                private: false,
            },
            Candidate {
                label: "TMPDIR",
                path: Some(usable.clone()),
                private: true,
            },
        ])
        .unwrap();
        assert_eq!(chosen, usable);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_documented_chain_order_is_what_ships() {
        let labels: Vec<_> = candidates().iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            ["NUB_COMPILE_CACHE_DIR", "XDG_CACHE_HOME", "HOME", "TMPDIR"]
        );
        // Only the temp candidate is private — it is the one with a
        // world-writable parent.
        let private: Vec<_> = candidates().iter().map(|c| c.private).collect();
        assert_eq!(private, [false, false, false, true]);
    }

    /// A directory another uid owns is a code-execution handoff — the launcher
    /// exec's the Node it extracts there. Root owns `/` on every unix, so it is
    /// the one foreign-owned directory a test can count on without privileges.
    #[cfg(unix)]
    #[test]
    fn a_foreign_owned_directory_is_refused() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(matches!(
            probe(Path::new("/"), false),
            Err(Reject::ForeignOwner)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_temp_candidate_is_per_uid() {
        let leaf = temp_leaf();
        assert_eq!(leaf, format!("nub-{}", unsafe { libc::geteuid() }));
        // A shared, guessable leaf is the CVE-2024-24828 shape.
        assert_ne!(leaf, "nub");
    }

    #[test]
    fn the_error_names_every_candidate_and_a_remedy() {
        let err = CacheError {
            attempts: vec![
                ("NUB_COMPILE_CACHE_DIR", None, Reject::Unset),
                (
                    "HOME",
                    Some(PathBuf::from("/root/.cache/nub")),
                    Reject::ReadOnly,
                ),
                ("TMPDIR", Some(PathBuf::from("/tmp/nub-0")), Reject::NoExec),
            ],
        };
        let msg = err.to_string();
        assert!(msg.contains("/root/.cache/nub"), "{msg}");
        assert!(msg.contains("read-only filesystem"), "{msg}");
        assert!(msg.contains("mounted noexec"), "{msg}");
        assert!(msg.contains("NUB_COMPILE_CACHE_DIR"), "{msg}");
        assert!(msg.contains("--smol"), "{msg}");
    }
}
