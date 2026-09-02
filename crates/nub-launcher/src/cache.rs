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
//! write, and (when Node runs from it) exec" by trying each candidate for real.
//!
//! The write probe is skipped for a candidate that already holds this run's
//! extracted artifacts. Such a run writes nothing, and demanding write access
//! anyway is precisely what broke the pre-warmed read-only deploy the error text
//! below recommends. The ownership gate always runs. The `noexec` gate runs only
//! when something is exec'd out of this cache — the Node binary, or an app file
//! the payload marked executable. A payload of ordinary data, loaded by a
//! separately discovered external Node, is safe on a noexec volume.
//!
//! Security posture: `vercel/pkg` extracted executable content to a hardcoded,
//! world-shared `/tmp/pkg/*`, which let a local attacker pre-seed the path and
//! get their code exec'd by someone else's binary (CVE-2024-24828). Every cache
//! root here is therefore resolved onto its canonical ancestor chain, created
//! component-by-component, and rejected when any principal other than this user
//! (or the platform administrator) can replace an entry in that chain. Unix's
//! sticky-directory rule keeps the per-uid temp fallback compatible without
//! making a shared non-sticky parent safe. Windows cache directories receive a
//! protected private DACL rather than inheriting a possibly-shared one. Extracted
//! payloads stay keyed by content hash, and the launcher revalidates the exact
//! runnable contents before accepting a completed cache. Processes already
//! running as this same user are trusted; excluding them would require binding
//! validation and execution to one open executable handle.

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
    /// Exists as a symlink or Windows reparse point where a real directory is
    /// required.
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
            Reject::Symlinked => write!(f, "is a symlink or reparse point"),
            Reject::Unusable(e) => write!(f, "{e}"),
        }
    }
}

struct Candidate {
    /// How the path is described in the error report.
    label: &'static str,
    path: Option<PathBuf>,
}

/// Whether this launch will execute anything out of the selected cache.
///
/// A compiled app still needs a private, writable cache for its payload, but a
/// `--smol` launcher that has already proved an external Node can execute that
/// Node outside the cache. In that one case a noexec cache is valid data storage
/// — provided the payload itself carries nothing executable, which the launcher
/// checks alongside the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheUse {
    Executable,
    DataOnly,
}

impl CacheUse {
    fn requires_exec(self) -> bool {
        matches!(self, Self::Executable)
    }
}

/// Every candidate that was tried and why each failed.
pub struct CacheError {
    attempts: Vec<(&'static str, Option<PathBuf>, Reject)>,
    use_: CacheUse,
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.use_ {
            CacheUse::Executable => {
                writeln!(f, "no writable, executable cache directory was found.")?
            }
            CacheUse::DataOnly => writeln!(f, "no writable cache directory was found.")?,
        }
        writeln!(f, "  Tried:")?;
        for (label, path, why) in &self.attempts {
            match path {
                Some(p) => writeln!(f, "    {label} ({}) — {why}", p.display())?,
                None => writeln!(f, "    {label} — {why}")?,
            }
        }
        match self.use_ {
            CacheUse::Executable => write!(
                f,
                "  Any one of these fixes it:\n\
                 \x20   - make one of the cache paths above writable and executable\n\
                 \x20   - mount a writable exec volume (Docker: --tmpfs /tmp:exec; \
                 Kubernetes: an emptyDir volume)\n\
                 \x20   - pre-warm a cache path into the image at build time, so the runtime \
                 hit is warm and writes nothing\n\
                 \x20   - build with --smol against a Node already installed on the box, \
                 which extracts no runtime"
            ),
            CacheUse::DataOnly => write!(
                f,
                "  Make one of the cache paths above writable, or pre-warm the compiled app \
                 into the image at build time so the runtime writes nothing."
            ),
        }
    }
}

impl fmt::Debug for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for CacheError {}

/// The selected cache root and, when the caller verified a complete read-only
/// hit while choosing it, the verified state to consume without repeating the
/// potentially expensive content checks.
pub struct Resolution<T> {
    pub path: PathBuf,
    pub warm: Option<T>,
}

/// The first candidate this process can actually create and write into. When
/// `use_` is [`CacheUse::Executable`], it must also permit execution; otherwise
/// it is app-data storage for an already-proven external Node. Order:
/// internal override → `$XDG_CACHE_HOME/nub` → `$HOME/.cache/nub` → a
/// per-uid directory under the temp dir.
///
/// `warm` is the caller's because only it knows the payload: the launcher checks
/// for the exact content-keyed artifacts its manifest names, so "warm" means
/// genuinely nothing left to write rather than "the directory is there".
pub fn resolve<T>(
    use_: CacheUse,
    warm: &dyn Fn(&Path) -> Option<T>,
) -> Result<Resolution<T>, CacheError> {
    resolve_from(candidates(), use_, warm)
}

/// Re-run the read-only namespace gate immediately before the launcher hands
/// cache paths to `Command`. A different principal cannot change a chain that
/// passed this gate, but the second check closes changes made by deployment
/// tooling between extraction and spawn without writing to a warm cache.
pub fn revalidate(path: &Path) -> io::Result<()> {
    inspect_existing(path).map_err(|error| io::Error::other(error.to_string()))?;
    fs::symlink_metadata(path).map(drop)
}

/// Validate one nested cache object against the Windows owner/DACL policy.
/// Callers first validate the cache root's full namespace with [`revalidate`],
/// then use this on every directory and file whose contents they trust.
#[cfg(windows)]
pub fn object_is_stable(path: &Path) -> io::Result<bool> {
    nub_core::windows_security::directory_is_stable(path, true, false)
}

#[cfg(all(test, windows))]
pub(crate) fn create_shared_test_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            "icacls could not grant Everyone full control",
        ))
    }
}

/// The chain, over an explicit candidate list so it can be driven in a test
/// without mutating process-global environment variables.
fn resolve_from<T>(
    candidates: Vec<Candidate>,
    use_: CacheUse,
    warm: &dyn Fn(&Path) -> Option<T>,
) -> Result<Resolution<T>, CacheError> {
    let mut attempts = Vec::new();
    for cand in candidates {
        let Some(path) = cand.path else {
            attempts.push((cand.label, None, Reject::Unset));
            continue;
        };
        let path = match canonical_cache_path(&path) {
            Ok(path) => path,
            Err(why) => {
                attempts.push((cand.label, Some(path), why));
                continue;
            }
        };
        // Never traverse a candidate to inspect its payload before the cache
        // root and every ancestor used to reach it pass the namespace gate.
        if let Err(why) = inspect_existing(&path) {
            attempts.push((cand.label, Some(path), why));
            continue;
        }
        let warm = warm(&path);
        let verdict = if warm.is_some() {
            probe_warm(&path, use_)
        } else {
            probe(&path, use_)
        };
        match verdict {
            Ok(()) => return Ok(Resolution { path, warm }),
            Err(why) => attempts.push((cand.label, Some(path), why)),
        }
    }
    Err(CacheError { attempts, use_ })
}

/// Where a `--smol` probe verdict is remembered, and the stamp that makes it
/// stale.
///
/// `--smol` discovers a Node it did not install, so it EXECUTES the candidate once
/// per launch to learn its real version — the directory name is only a hint, and
/// that verdict gates whether the app cache may be relaxed to data-only. A `node`
/// spawn is ~28 ms and accounts for the entire warm gap between `--smol` and the
/// embed shape (66.3 vs 41.7 ms, measured), so it is worth not repeating.
///
/// Remembering it is sound for the same reason the per-launch digest was dropped:
/// this file lives in the invoking uid's own write domain, and that uid can replace
/// the `node` binary itself far more cheaply than it can doctor a cache entry. The
/// entry is therefore not the weakest link, and it is validated as owner-only
/// before being believed.
///
/// The stamp is `mtime + size`, whose known failure is two different files sharing
/// both — the same shape as the `ROOT_MANIFEST_CACHE` flake this repo already hit
/// on Windows. Distinct Node builds differ in size, so a collision here would need
/// a same-size rebuild written within one filesystem timestamp tick; a wrong
/// verdict then costs one launch on a mislabelled Node, not a safety property.
const PROBE_DIR: &str = "smol-probe";

/// A remembered probe verdict for `node_bin`, if one exists and still applies.
///
/// Returns `None` on any doubt — a missing, untrusted, malformed, or stale entry
/// all mean "probe it again", which is always correct and merely slower.
pub fn read_node_version(node_bin: &Path, stamp: &str) -> Option<String> {
    for candidate in candidates() {
        let Some(base) = candidate.path else { continue };
        let path = probe_path(&base, node_bin);
        // The namespace gate walks DIRECTORIES, so it is applied to the directory
        // holding the entry; the entry itself is then checked directly. Handing it
        // the file made every read miss, silently — the cache wrote and never hit.
        let Some(parent) = path.parent() else {
            continue;
        };
        if inspect_existing(parent).is_err() {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        // symlink_metadata, so a symlink planted at this name is rejected rather
        // than followed to whatever it points at.
        if !metadata.is_file() || !owner_only(&metadata) {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        // `<stamp>\n<version>`: the stamp must match the binary as it is NOW, or
        // this entry describes a different file that happened to sit at this path.
        let Some((recorded, version)) = body.split_once('\n') else {
            continue;
        };
        if recorded == stamp {
            return Some(version.trim().to_string());
        }
    }
    None
}

/// Remember a probe verdict. Best effort: a cache that cannot be written is a
/// slower launch, never a failed one, so every error here is discarded.
///
/// The probe directory is made by `create`, never `create_dir_all`, and the two
/// are not interchangeable here: `create_dir_all` takes the directory's mode from
/// the ambient umask, and [`read_node_version`] runs the same leaf gate that
/// rejects any group- or other-writable directory. Under a umask of 002 that
/// `create_dir_all` yields 0o775, so every write lands in a directory every read
/// then refuses — the cache writes and never hits, silently, exactly the failure
/// the parent-vs-leaf note on the read path already records once.
pub fn write_node_version(node_bin: &Path, stamp: &str, version: &str) {
    for candidate in candidates() {
        let Some(base) = candidate.path else { continue };
        let path = probe_path(&base, node_bin);
        let Some(parent) = path.parent() else {
            continue;
        };
        if create(parent).is_err() {
            continue;
        }
        if fs::write(&path, format!("{stamp}\n{version}\n")).is_ok() {
            let _ = harden_probe(&path);
            return;
        }
    }
}

/// One file per probed binary, named by the hash of its path so an arbitrary
/// filesystem path becomes a fixed-width name that cannot escape the directory.
fn probe_path(base: &Path, node_bin: &Path) -> PathBuf {
    let name = blake3::hash(node_bin.to_string_lossy().as_bytes()).to_hex();
    base.join(PROBE_DIR).join(name.as_str())
}

#[cfg(unix)]
fn owner_only(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn owner_only(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn harden_probe(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_probe(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn candidates() -> Vec<Candidate> {
    vec![
        // An explicit override is used verbatim — no `nub` subdirectory appended.
        // This is internal launcher plumbing rather than a user-facing knob.
        Candidate {
            label: "configured cache directory",
            path: non_empty_var("__NUB_COMPILE_CACHE_DIR").map(PathBuf::from),
        },
        Candidate {
            label: "XDG_CACHE_HOME",
            path: non_empty_var("XDG_CACHE_HOME").map(|v| PathBuf::from(v).join("nub")),
        },
        Candidate {
            label: "HOME",
            path: home_cache(),
        },
        Candidate {
            label: "TMPDIR",
            path: Some(std::env::temp_dir().join(temp_leaf())),
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

/// Resolve redirects only in the candidate's existing parent prefix, then use
/// the resolved spelling for every later operation. This preserves legitimate
/// homes/cache volumes reached through a stable symlink (macOS `/var` is one)
/// without leaving a traversed redirect in the executable cache pathname.
///
/// Missing suffix components are appended after canonicalization and are later
/// created one at a time behind the platform namespace gate.
fn canonical_cache_path(path: &Path) -> Result<PathBuf, Reject> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(classify)?.join(path)
    };
    let leaf = absolute
        .file_name()
        .ok_or_else(|| Reject::Unusable(io::Error::other("cache path has no directory leaf")))?;
    let mut existing = absolute
        .parent()
        .ok_or_else(|| Reject::Unusable(io::Error::other("cache path has no parent")))?
        .to_path_buf();
    let mut missing = Vec::new();

    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| classify(error))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| classify(io::Error::from(io::ErrorKind::NotFound)))?
                    .to_path_buf();
            }
            Err(error) => return Err(classify(error)),
        }
    }

    let mut resolved = fs::canonicalize(existing).map_err(classify)?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    resolved.push(leaf);
    Ok(resolved)
}

/// Atomically create the directory if needed, verify it without following a
/// symlink, and write a probe file. When this cache supplies Node, also confirm
/// the mount permits exec. Every step is an action, not an assumption — the whole
/// point is that `Some(path)` proves nothing.
fn probe(path: &Path, use_: CacheUse) -> Result<(), Reject> {
    create(path)?;
    write_probe(path).map_err(classify)?;
    if use_.requires_exec() {
        exec_gate(path)?;
    }
    Ok(())
}

/// [`probe`] minus the two steps that write. A warm candidate is only being read
/// from, so requiring write access would reject a correct deployment. The exec
/// gate still applies only when the selected cache supplies the Node executable.
fn probe_warm(path: &Path, use_: CacheUse) -> Result<(), Reject> {
    inspect_existing(path)?;
    if use_.requires_exec() {
        exec_gate(path)?;
    }
    Ok(())
}

fn exec_gate(path: &Path) -> Result<(), Reject> {
    if mount_is_noexec(path) {
        return Err(Reject::NoExec);
    }
    Ok(())
}

/// No-follow ownership + namespace gate on a directory that already exists.
///
/// Foreign ownership disqualifies every candidate: the launcher exec's a binary
/// out of this tree, so a directory another uid controls is a code-execution
/// handoff. Group- and other-write disqualify every candidate: mode bits do not
/// prove that a writable group contains only the current user. The same rule is
/// enforced at every ancestor that controls a pathname component. A sticky
/// shared parent is safe only for an existing child owned by this uid (or for an
/// atomic child mkdir that is immediately checked that way), which is the Unix
/// `/tmp/nub-<uid>` contract.
#[cfg(unix)]
fn inspect_existing(path: &Path) -> Result<(), Reject> {
    walk_unix_namespace(path, false)
}

/// Windows uses its DACL rather than Unix mode bits. Every existing component is
/// checked after ancestor redirects have been canonicalized away; the final leaf
/// must be owned by the current user (or the administrator/system boundary) and
/// must not grant another SID mutation or delete rights.
#[cfg(windows)]
fn inspect_existing(path: &Path) -> Result<(), Reject> {
    walk_windows_namespace(path, false)
}

#[cfg(not(any(unix, windows)))]
fn inspect_existing(path: &Path) -> Result<(), Reject> {
    let md = match fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(classify(e)),
    };
    if md.file_type().is_symlink() {
        return Err(Reject::Symlinked);
    }
    Ok(())
}

/// Windows marks junctions and other namespace redirects with this bit, even
/// when [`std::fs::FileType::is_symlink`] does not report them as symlinks.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

#[cfg(windows)]
fn has_windows_reparse_point(file_attributes: u32) -> bool {
    file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn walk_unix_namespace(path: &Path, create_missing: bool) -> Result<(), Reject> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let euid = unsafe { libc::geteuid() };
    let mut chain: Vec<_> = path.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    let mut parent: Option<fs::Metadata> = None;

    for (index, component) in chain.iter().enumerate() {
        let is_leaf = index + 1 == chain.len();
        let mut metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(classify(error)),
        };

        if let Some(parent) = &parent {
            unix_parent_protects_child(parent, metadata.as_ref(), euid)?;
        }

        if metadata.is_none() {
            if !create_missing {
                return Ok(());
            }
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            if let Err(error) = builder.create(component)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(classify(error));
            }
            metadata = Some(fs::symlink_metadata(component).map_err(classify)?);
            if let Some(parent) = &parent {
                unix_parent_protects_child(parent, metadata.as_ref(), euid)?;
            }
        }

        let metadata = metadata.expect("created or existing namespace component");
        if metadata.file_type().is_symlink() {
            return Err(Reject::Symlinked);
        }
        if !metadata.file_type().is_dir() {
            return Err(Reject::Unusable(io::Error::from(
                io::ErrorKind::NotADirectory,
            )));
        }
        if is_leaf {
            if metadata.uid() != euid {
                return Err(Reject::ForeignOwner);
            }
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(Reject::Shared);
            }
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
) -> Result<(), Reject> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if parent.file_type().is_symlink() {
        return Err(Reject::Symlinked);
    }
    if !parent.file_type().is_dir() {
        return Err(Reject::Unusable(io::Error::from(
            io::ErrorKind::NotADirectory,
        )));
    }

    let mode = parent.permissions().mode();
    // A foreign, non-root owner can chmod its directory and then rename children
    // regardless of its present mode. uid 0 is the Unix administrator boundary,
    // not a peer principal the cache can defend against.
    if parent.uid() != euid && parent.uid() != 0 {
        return Err(Reject::ForeignOwner);
    }
    if mode & 0o022 == 0 {
        return Ok(());
    }
    if mode & 0o1000 == 0 {
        return Err(Reject::Shared);
    }

    // Sticky directories protect an existing name only for its owner. A missing
    // child is safe to create atomically; the post-mkdir call checks who won.
    if let Some(child) = child
        && child.uid() != euid
    {
        return Err(Reject::ForeignOwner);
    }
    Ok(())
}

#[cfg(windows)]
fn walk_windows_namespace(path: &Path, create_missing: bool) -> Result<(), Reject> {
    use std::os::windows::fs::MetadataExt;

    let mut chain: Vec<_> = path.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    for (index, component) in chain.iter().enumerate() {
        let is_leaf = index + 1 == chain.len();
        let is_volume_root = index == 0;
        let mut metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(classify(error)),
        };
        if metadata.is_none() {
            if !create_missing {
                return Ok(());
            }
            if let Err(error) = nub_core::windows_security::create_private_directory(component)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(classify(error));
            }
            metadata = Some(fs::symlink_metadata(component).map_err(classify)?);
        }

        let metadata = metadata.expect("created or existing namespace component");
        if metadata.file_type().is_symlink()
            || has_windows_reparse_point(metadata.file_attributes())
        {
            return Err(Reject::Symlinked);
        }
        if !metadata.file_type().is_dir() {
            return Err(Reject::Unusable(io::Error::from(
                io::ErrorKind::NotADirectory,
            )));
        }
        if !nub_core::windows_security::directory_is_stable(component, is_leaf, is_volume_root)
            .map_err(classify)?
        {
            return Err(if is_leaf {
                Reject::Shared
            } else {
                Reject::ForeignOwner
            });
        }
    }
    Ok(())
}

/// Create every missing component atomically behind the same namespace checks
/// used for warm reads. No `create_dir_all`: it follows redirects and says
/// nothing about who can rename an otherwise-private final leaf.
#[cfg(unix)]
fn create(path: &Path) -> Result<(), Reject> {
    walk_unix_namespace(path, true)
}

#[cfg(windows)]
fn create(path: &Path) -> Result<(), Reject> {
    walk_windows_namespace(path, true)
}

#[cfg(not(any(unix, windows)))]
fn create(path: &Path) -> Result<(), Reject> {
    fs::create_dir_all(path).map_err(classify)?;
    inspect_existing(path)
}

/// `create_dir_all` succeeding on an existing directory says nothing about
/// writability, so write a real file.
fn write_probe(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".nub-write-probe.{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let result = options.open(&probe).map(drop);
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
#[cfg(unix)]
pub fn exec_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

/// The remedies for a `noexec` cache directory, appended to the exec failure.
#[cfg(unix)]
pub fn noexec_remedy(dir: &Path) -> String {
    format!(
        "the cache directory {} is on a filesystem mounted noexec, so the Node \
         runtime extracted there cannot be executed.\n\
         \x20 Any one of these fixes it:\n\
         \x20   - use a cache filesystem that permits exec\n\
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&d).unwrap();
        }
        #[cfg(windows)]
        nub_core::windows_security::create_private_directory(&d).unwrap();
        #[cfg(not(any(unix, windows)))]
        fs::create_dir(&d).unwrap();
        // Production candidates are canonicalized before any no-follow probe.
        // Keep direct probe tests on that same side of the boundary: macOS's
        // temp directory is spelled through the stable `/var` redirect.
        fs::canonicalize(d).unwrap()
    }

    #[test]
    fn probe_creates_and_accepts_a_writable_directory() {
        let root = scratch("writable");
        let target = root.join("missing").join("parents").join("cache");
        assert!(probe(&target, CacheUse::Executable).is_ok());
        assert!(target.is_dir());
        // The probe file must not survive the probe.
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn probe_creates_every_cache_directory_0700() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("cache-mode");
        let target = root.join("cache");
        probe(&target, CacheUse::Executable).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "cache dir must be 0700, got {mode:o}");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn probe_refuses_a_group_or_world_writable_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("shared-perms");

        for mode in [0o777, 0o775, 0o770, 0o757, 0o707] {
            let target = root.join(format!("d{mode:o}"));
            fs::create_dir_all(&target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
            let err = probe(&target, CacheUse::Executable).unwrap_err();
            assert!(
                matches!(err, Reject::Shared),
                "mode {mode:o} must be refused as shared, got {err}"
            );
        }

        let readable = root.join("world-readable");
        fs::create_dir_all(&readable).unwrap();
        fs::set_permissions(&readable, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            probe(&readable, CacheUse::Executable).is_ok(),
            "read and exec bits do not let another principal replace cached executables"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn private_leaf_under_non_sticky_shared_parent_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("shared-ancestor");
        for mode in [0o777, 0o770] {
            let parent = root.join(format!("parent-{mode:o}"));
            let leaf = parent.join("victim-cache");
            fs::create_dir(&parent).unwrap();
            fs::create_dir(&leaf).unwrap();
            fs::set_permissions(&leaf, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).unwrap();

            let error = probe(&leaf, CacheUse::Executable).unwrap_err();
            assert!(
                matches!(error, Reject::Shared),
                "a {mode:o} ancestor can rename its 0700 child, got {error}"
            );
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn sticky_shared_parent_accepts_an_atomic_per_uid_leaf() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("sticky-ancestor");
        let parent = root.join("tmp");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).unwrap();
        let leaf = parent.join(format!("nub-{}", unsafe { libc::geteuid() }));

        probe(&leaf, CacheUse::Executable).unwrap();
        let metadata = fs::symlink_metadata(&leaf).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn stable_ancestor_symlink_is_canonicalized_out_of_the_cache_path() {
        use std::os::unix::fs::DirBuilderExt;

        let root = scratch("canonical-parent");
        let real = root.join("real");
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let requested = link.join("cache");

        let canonical = canonical_cache_path(&requested).unwrap();
        assert_eq!(canonical, real.join("cache"));
        probe(&canonical, CacheUse::Executable).unwrap();
        assert!(real.join("cache").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn probe_never_follows_a_symlinked_cache_directory() {
        let root = scratch("symlink");
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            probe(&link, CacheUse::Executable),
            Err(Reject::Symlinked)
        ));
        assert_eq!(
            fs::read_dir(&real).unwrap().count(),
            0,
            "the write probe must not land through the symlink"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_cache_root_is_rejected_before_callback_or_write_probe() {
        use std::os::windows::fs::symlink_dir;
        use std::process::Command;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = scratch("windows-reparse");
        let target = root.join("target");
        let reparse = root.join("cache");
        fs::create_dir(&target).unwrap();

        // Directory junctions are the unprivileged Windows reparse primitive.
        // If policy disables them, use the standard-library directory-symlink
        // fallback, which may need Developer Mode or symlink privilege.
        let junction = Command::new("cmd")
            .arg("/C")
            .arg(format!(
                "mklink /J \"{}\" \"{}\"",
                reparse.display(),
                target.display()
            ))
            .output()
            .unwrap_or_else(|error| panic!("starting cmd to create directory junction: {error}"));
        if !junction.status.success() {
            if let Err(error) = symlink_dir(&target, &reparse) {
                if error.raw_os_error() == Some(1314) {
                    eprintln!(
                        "skipping Windows cache reparse rejection: directory junction creation failed and symlink privilege is unavailable"
                    );
                    let _ = fs::remove_dir_all(&root);
                    return;
                }
                panic!(
                    "creating Windows directory reparse point (junction status {:?}; symlink error: {error})",
                    junction.status.code()
                );
            }
        }

        let warm_calls = AtomicUsize::new(0);
        let result = resolve_from(
            vec![Candidate {
                label: "configured cache directory",
                path: Some(reparse.clone()),
            }],
            CacheUse::Executable,
            &|_: &Path| {
                warm_calls.fetch_add(1, Ordering::SeqCst);
                None::<()>
            },
        );
        assert!(result.is_err(), "directory reparse cache root was accepted");
        assert_eq!(
            warm_calls.load(Ordering::SeqCst),
            0,
            "cache resolution must reject the reparse root before reading it"
        );
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "cache resolution must not write through the reparse root"
        );
        fs::remove_dir(&reparse).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_shared_dacl_cache_leaf_is_refused() {
        let root = scratch("windows-shared-dacl");
        let leaf = root.join("cache");
        create_shared_test_directory(&leaf).unwrap();

        assert!(
            matches!(probe(&leaf, CacheUse::Executable), Err(Reject::Shared)),
            "a normal directory writable through an Everyone DACL was accepted"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_created_cache_leaf_has_a_private_stable_dacl() {
        let root = scratch("windows-private-dacl");
        let leaf = root.join("cache");

        probe(&leaf, CacheUse::Executable).unwrap();
        assert!(
            nub_core::windows_security::directory_is_stable(&leaf, true, false).unwrap(),
            "created cache directory did not receive the protected private DACL"
        );
        fs::remove_dir_all(&root).unwrap();
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
        assert!(matches!(
            probe(&target, CacheUse::Executable),
            Err(Reject::Unusable(_))
        ));
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

        let chosen = resolve_from(
            vec![
                Candidate {
                    label: "configured cache directory",
                    path: None,
                },
                Candidate {
                    label: "HOME",
                    path: Some(locked.clone()),
                },
                Candidate {
                    label: "TMPDIR",
                    path: Some(usable.clone()),
                },
            ],
            CacheUse::Executable,
            &|_: &Path| None::<()>,
        )
        .unwrap();
        assert_eq!(chosen.path, canonical_cache_path(&usable).unwrap());
        assert!(chosen.warm.is_none());

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// The deployment the error text itself recommends — a cache pre-warmed into
    /// an image layer, then run read-only. The same unwritable directory must be
    /// accepted when the run needs no writes and refused when it does, so the
    /// pair is asserted together: only the warm signal differs between them.
    #[cfg(unix)]
    #[test]
    fn a_warm_candidate_is_accepted_without_write_access() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = scratch("warm-readonly");
        let locked = root.join("prewarmed");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

        let resolve_with = |warm: bool| {
            resolve_from(
                vec![Candidate {
                    label: "configured cache directory",
                    path: Some(locked.clone()),
                }],
                CacheUse::Executable,
                &|_: &Path| warm.then_some(()),
            )
        };
        let resolved = resolve_with(true).unwrap();
        assert_eq!(resolved.path, canonical_cache_path(&locked).unwrap());
        assert!(resolved.warm.is_some());
        assert!(
            resolve_with(false).is_err(),
            "a run that still has to extract must prove it can write"
        );

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn data_only_cache_does_not_require_exec_but_executable_cache_does() {
        assert!(!CacheUse::DataOnly.requires_exec());
        assert!(CacheUse::Executable.requires_exec());
    }

    #[test]
    fn data_only_failure_does_not_recommend_smol_or_exec_storage() {
        let error = CacheError {
            use_: CacheUse::DataOnly,
            attempts: vec![("TMPDIR", Some(PathBuf::from("/tmp/nub-0")), Reject::NoExec)],
        };
        let message = error.to_string();
        assert!(message.contains("no writable cache directory"), "{message}");
        assert!(!message.contains("executable cache"), "{message}");
        assert!(!message.contains("--smol"), "{message}");
    }

    #[test]
    fn the_documented_chain_order_is_what_ships() {
        let labels: Vec<_> = candidates().iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            [
                "configured cache directory",
                "XDG_CACHE_HOME",
                "HOME",
                "TMPDIR"
            ]
        );
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
            probe(Path::new("/"), CacheUse::Executable),
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

    /// A remembered probe verdict is returned only for the binary it describes.
    ///
    /// The hit half alone would pass against a cache that ignored the stamp
    /// entirely, which is the dangerous failure: a replaced Node keeps its old
    /// verdict and the artifact runs a version it never verified. So the stale
    /// stamp is asserted too, and it is the assertion that matters.
    ///
    /// Read misses are always safe — they cost one exec — so every rejection path
    /// here is the slow answer, never a wrong one.
    #[cfg(unix)]
    #[test]
    fn a_probe_verdict_is_reused_only_while_the_binary_is_unchanged() {
        let base = scratch("probe-cache");
        // SAFETY: read-only; `set_var` is what makes `candidates()` resolve here.
        unsafe { std::env::set_var("__NUB_COMPILE_CACHE_DIR", &base) };

        let node = base.join("node");
        fs::write(&node, b"#!/bin/sh\nexit 0\n").unwrap();

        write_node_version(&node, "stamp-A", "26.5.0");
        assert_eq!(
            read_node_version(&node, "stamp-A").as_deref(),
            Some("26.5.0"),
            "a verdict written for this stamp must come back"
        );
        assert_eq!(
            read_node_version(&node, "stamp-B"),
            None,
            "a DIFFERENT stamp means the binary changed — the old verdict must not \
             be reused, or a replaced Node inherits a version nobody verified"
        );

        // A different binary path must not read this one's verdict, since the
        // entry is named by the path's hash.
        let other = base.join("other-node");
        fs::write(&other, b"#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            read_node_version(&other, "stamp-A"),
            None,
            "verdicts are per binary path, never shared"
        );

        // The DIRECTORY's mode is pinned, never inherited from the umask. The read
        // path runs the leaf gate on it, so a `create_dir_all` under a umask of 002
        // (0o775) would make every write unreadable — the cache writing and never
        // hitting, on every machine with that umask, invisible from a 022 one.
        // Asserted here rather than in a test of its own because this one already
        // owns `__NUB_COMPILE_CACHE_DIR`, and a second writer of that process global
        // would race it under the parallel runner.
        //
        // The assertion pins the EXACT mode rather than testing `& 0o022 == 0`, and
        // that difference is the whole value of it: `create_dir_all` yields
        // `0o777 & !umask`, so the weaker form holds at the 0o755 a 022 umask gives
        // and stays green on CI — passing on the very environment that hid the bug.
        // Only 0o700 discriminates, and it is umask-independent precisely because it
        // has no group or other bits left for a umask to clear.
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = probe_path(&base, &node).parent().unwrap().to_path_buf();
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "the probe directory's mode must be pinned, not taken from the umask: {mode:o}"
            );
        }

        // A group-writable entry is somebody else's to edit, so it is not believed.
        {
            use std::os::unix::fs::PermissionsExt;
            let entry = probe_path(&base, &node);
            fs::set_permissions(&entry, fs::Permissions::from_mode(0o666)).unwrap();
            assert_eq!(
                read_node_version(&node, "stamp-A"),
                None,
                "a world-writable verdict must be refused"
            );
        }

        unsafe { std::env::remove_var("__NUB_COMPILE_CACHE_DIR") };
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_error_names_every_candidate_and_a_remedy() {
        let err = CacheError {
            use_: CacheUse::Executable,
            attempts: vec![
                ("configured cache directory", None, Reject::Unset),
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
        assert!(!msg.contains("NUB_COMPILE_CACHE_DIR"), "{msg}");
        assert!(msg.contains("--smol"), "{msg}");
    }
}
