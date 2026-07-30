//! Retained-monitor bootstrap protocol and pinned runtime image for Linux.
//!
//! The monitor is the exact embedder executable, re-entered before normal program
//! initialization and made PID 1 by stock Bubblewrap.  This module owns the
//! environment-independent states 1-5 bootstrap, stopped-target handshake, and
//! one-shot exec transition, plus the pinned ELF runtime closure and framed control
//! channel, runtime signal relay, exact terminal-status reporting, and namespace
//! process-tree cleanup. The production confined-Linux path (`backend::linux::apply`)
//! constructs and adopts this launcher unconditionally; the sealed hold flags on
//! `BootstrapSpec` let the harness independently exercise states 5-8 of the same
//! protocol against a real child process.
#![cfg(target_os = "linux")]

use super::{CommandSpec, Degradation};
use crate::policy::SandboxPolicy;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const MONITOR_SENTINEL: &str = "__nub-sandbox-monitor-v1";
const DESCRIBE_SENTINEL: &str = "__nub-sandbox-describe-runtime-v1";
const PROTOCOL_VERSION: u16 = 1;
const BOOT_MAGIC: &[u8; 8] = b"NUBBOOT1";
const FRAME_MAGIC: &[u8; 8] = b"NUBMON01";
const DESCRIBE_MAGIC: &[u8; 8] = b"NUBMAP01";
const MAX_BOOTSTRAP_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAME_PAYLOAD: usize = 64 * 1024;
const MAX_VECTOR_ITEMS: usize = 65_536;
const MAX_RUNTIME_OBJECTS: usize = 256;
const MAX_MAPPED_CANDIDATES: usize = 1024;
const MAX_MAPS_BYTES: usize = 4 * 1024 * 1024;
const MAX_RUNTIME_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const PRIVATE_RUNTIME_ROOT: &str = "/dev/.nub-sandbox/runtime";
pub(crate) const PRIVATE_SUPPORT_ROOT: &str = "/dev/.nub-sandbox/support";
pub(crate) const PRIVATE_CA_BUNDLE: &str = "/dev/.nub-sandbox/support/ca-bundle.pem";
pub(crate) const CANDIDATE_PROBE_SENTINEL: &str = "__nub-sandbox-candidate-probe-v1";
/// C3: the FIXED loopback port the in-netns egress bridge binds inside the sandbox, and the
/// port the child's proxy env points at. Each `--unshare-net` child has a PRIVATE empty
/// netns, so a fixed port never collides across children or nesting levels; the bridge binds
/// it before the target is released, so the target can never win the bind race.
pub(crate) const IN_NETNS_PROXY_PORT: u16 = 47981;
const LINUX_NSIG: libc::c_int = 65;
const REQUIRED_BOOTSTRAP_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
const TARGET_SETUP_ERROR_MAGIC: &[u8; 4] = b"NTE1";
const TARGET_SETUP_ERROR_LEN: usize = 12;
const START_CHALLENGE_LEN: usize = 32;
const START_GATE_BYTE: u8 = 0xa5;
const TARGET_EXEC_FAILURE_LEN: usize = 12;
const TARGET_EXITED_LEN: usize = 8;
const COMPLETION_CHALLENGE_LEN: usize = 32;
const COMPLETION_ATTESTATION_LEN: usize = TARGET_EXITED_LEN + COMPLETION_CHALLENGE_LEN;
const SIGNAL_RELAY_MAGIC: &[u8; 4] = b"NSG1";
const SIGNAL_RELAY_RECORD_LEN: usize = 16;
const MAX_RUNNING_EVENTS_PER_TURN: usize = 64;

// These descriptors exist only in the post-fork Bubblewrap child.  The trusted
// parent first duplicates every source above this reserved range, then remaps
// those child-local copies in pre_exec, so remap cycles cannot clobber a source
// and no caller-owned descriptor is overwritten in the long-lived Nub process.
pub(crate) const BOOTSTRAP_FD: RawFd = 198;
pub(crate) const CONTROL_FD: RawFd = 199;
pub(crate) const RELEASE_FD: RawFd = 200;
pub(crate) const SIGNAL_RELAY_FD: RawFd = 201;
const FIRST_UNRESERVED_MONITOR_FD: RawFd = SIGNAL_RELAY_FD + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) size: u64,
}

impl FileIdentity {
    pub(super) fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sandbox monitor runtime object is not a regular file",
            ));
        }
        if metadata.len() > MAX_RUNTIME_OBJECT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sandbox monitor runtime object exceeds the size budget",
            ));
        }
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
        })
    }
}

pub(crate) struct PinnedObject {
    pub(crate) file: File,
    pub(crate) source_path: PathBuf,
    pub(crate) identity: FileIdentity,
    pub(crate) private_name: OsString,
}

impl std::fmt::Debug for PinnedObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedObject")
            .field("source_path", &self.source_path)
            .field("identity", &self.identity)
            .field("private_name", &self.private_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoaderFamily {
    Glibc,
    Musl,
}

#[derive(Debug)]
pub(crate) enum RuntimeKind {
    Static,
    Dynamic {
        loader: PinnedObject,
        family: LoaderFamily,
        libraries: Vec<PinnedObject>,
        inhibit_rpath: OsString,
    },
}

#[derive(Debug)]
pub(crate) struct RuntimeImage {
    pub(crate) executable: PinnedObject,
    pub(crate) kind: RuntimeKind,
    pub(crate) build_marker: [u8; 32],
    /// Owns the staging directory when any object had to be re-anchored there. Every
    /// launch resolves the bind sources by path, so it must outlive the image, not the
    /// launch that created it.
    _stage: Option<super::linux_runtime_stage::StagedRuntime>,
}

/// Opaque, non-cloneable authority returned by the embedder's earliest hook.
/// Descriptor authority is captured synchronously before application threads;
/// full ELF closure parsing remains lazy until confinement consumes the token.
pub struct RuntimeCapability {
    source: RuntimeSource,
}

enum RuntimeSource {
    Current {
        /// The capture is EAGER — it must observe the pristine process image before any
        /// application code runs, which is the anti-substitution property itself. Its
        /// FAILURE is deferred, because this authority is consumed only by the Bubblewrap
        /// launch: a capture that could not complete must fail THAT launch, not the
        /// process. Landlock confines with no runtime image at all and legitimately
        /// cannot read `/proc/self/maps` — its ruleset is built pre-`fork`, so
        /// `/proc/self` is ungrantable — and every nub process nested inside a build jail
        /// re-enters this hook. Failing eagerly killed each one before its work began.
        authority: Result<EarlyRuntimeAuthority, CaptureFailure>,
        image: Box<OnceLock<Result<RuntimeImage, CaptureFailure>>>,
    },
    Explicit(RuntimeImage),
}

struct EarlyRuntimeAuthority {
    executable: PinnedObject,
    loader: Option<PinnedObject>,
    loader_path: Option<PathBuf>,
    inventory: Vec<PinnedCandidate>,
}

struct PinnedCandidate {
    file: File,
    source_path: PathBuf,
    identity: FileIdentity,
    aliases: BTreeSet<OsString>,
}

impl std::fmt::Debug for EarlyRuntimeAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EarlyRuntimeAuthority")
            .field("executable", &self.executable)
            .field("loader", &self.loader)
            .field("loader_path", &self.loader_path)
            .field("inventory_len", &self.inventory.len())
            .finish()
    }
}

#[derive(Debug)]
struct CaptureFailure {
    kind: io::ErrorKind,
    message: String,
}

impl CaptureFailure {
    fn from_io(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn to_io(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl std::fmt::Debug for RuntimeCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            RuntimeSource::Current { authority, image } => f
                .debug_struct("RuntimeCapability")
                .field("source", &"current-process")
                .field("authority", authority)
                .field("materialized", &image.get().is_some())
                .finish(),
            RuntimeSource::Explicit(image) => f
                .debug_struct("RuntimeCapability")
                .field("source", &"explicit-executable")
                .field("image", image)
                .finish(),
        }
    }
}

impl RuntimeCapability {
    fn current_process() -> Self {
        Self {
            source: RuntimeSource::Current {
                authority: capture_early_current_authority().map_err(CaptureFailure::from_io),
                image: Box::new(OnceLock::new()),
            },
        }
    }

    pub(crate) fn materialize(&self) -> io::Result<&RuntimeImage> {
        match &self.source {
            RuntimeSource::Current { authority, image } => {
                // Replays the deferred capture error verbatim. The caller
                // (`linux::preflight`) turns it into a `Degradation`, which the build jail
                // reports fail-closed — so nothing that needed this authority proceeds
                // without it.
                let authority = authority.as_ref().map_err(CaptureFailure::to_io)?;
                image
                    .get_or_init(|| {
                        materialize_early_authority(authority).map_err(CaptureFailure::from_io)
                    })
                    .as_ref()
                    .map_err(CaptureFailure::to_io)
            }
            RuntimeSource::Explicit(image) => Ok(image),
        }
    }

    /// Test/embedder-only explicit monitor executable.  It must implement the
    /// describe/bootstrap sentinels and is immediately pinned before description.
    #[doc(hidden)]
    pub fn from_verified_executable(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        let executable = pin_path(&path, OsStr::new("nub-monitor"))?;
        let inventory = describe_runtime(&executable)?;
        let authority = early_authority_from_inventory(executable, inventory)?;
        let image = materialize_early_authority(&authority)?;
        Ok(Self {
            source: RuntimeSource::Explicit(image),
        })
    }
}

fn capture_early_current_authority() -> io::Result<EarlyRuntimeAuthority> {
    let executable = pin_path(Path::new("/proc/self/exe"), OsStr::new("nub-monitor"))?;
    let maps = read_maps_snapshot()?;
    let inventory = pinned_inventory_from_maps(&maps)?;
    early_authority_from_inventory(executable, inventory)
}

fn early_authority_from_inventory(
    executable: PinnedObject,
    inventory: Vec<PinnedCandidate>,
) -> io::Result<EarlyRuntimeAuthority> {
    require_pinned_identity(&inventory, executable.identity, "monitor executable")?;
    let interpreter = parse_elf_interpreter(&executable.file)?;
    let (loader, loader_path) = interpreter
        .map(|interpreter| {
            let loader_path = PathBuf::from(OsString::from_vec(interpreter));
            if !loader_path.is_absolute() {
                return Err(invalid_data("ELF PT_INTERP is not absolute"));
            }
            let loader = pin_path(&loader_path, OsStr::new("ld.so"))?;
            require_pinned_identity(&inventory, loader.identity, "dynamic loader")?;
            Ok((loader, loader_path))
        })
        .transpose()?
        .map_or((None, None), |(loader, path)| (Some(loader), Some(path)));
    Ok(EarlyRuntimeAuthority {
        executable,
        loader,
        loader_path,
        inventory,
    })
}

fn materialize_early_authority(authority: &EarlyRuntimeAuthority) -> io::Result<RuntimeImage> {
    let mut executable = duplicate_pinned(&authority.executable)?;
    let inventory = parse_pinned_inventory(&authority.inventory)?;
    let parsed = parse_elf(&executable.file)?;
    let mut kind = match (
        &parsed.interpreter,
        &authority.loader,
        &authority.loader_path,
    ) {
        (None, None, None) => {
            validate_static_image(&parsed)?;
            RuntimeKind::Static
        }
        (Some(interpreter), Some(early_loader), Some(early_loader_path)) => {
            if !parsed.has_dynamic {
                return Err(invalid_data("ELF PT_INTERP has no dynamic table"));
            }
            let loader_path = PathBuf::from(OsString::from_vec(interpreter.clone()));
            if !loader_path.is_absolute() {
                return Err(invalid_data("ELF PT_INTERP is not absolute"));
            }
            if &loader_path != early_loader_path {
                return Err(invalid_data("ELF PT_INTERP changed after early capture"));
            }
            let loader = duplicate_pinned(early_loader)?;
            let loader_elf = parse_elf(&loader.file)?;
            validate_loader_image(&loader_elf)?;
            let family = loader_family(&loader_path)?;
            require_proven_loader_search(family)?;
            let (libraries, inhibit_rpath) =
                resolve_needed_closure(&[&parsed, &loader_elf], &inventory)?;
            RuntimeKind::Dynamic {
                loader,
                family,
                libraries,
                inhibit_rpath,
            }
        }
        _ => return Err(invalid_data("ELF PT_INTERP changed after early capture")),
    };
    // Re-anchoring comes AFTER every identity check above, which validates the objects
    // against the live mapping inventory, and BEFORE the manifest — so the marker the
    // monitor re-derives inside the sandbox records what Bubblewrap will actually bind.
    let mut staging = super::linux_runtime_stage::RuntimeStaging::new();
    staging.reanchor(&mut executable);
    if let RuntimeKind::Dynamic {
        loader, libraries, ..
    } = &mut kind
    {
        staging.reanchor(loader);
        for library in libraries {
            staging.reanchor(library);
        }
    }
    let objects = runtime_objects_from_parts(&executable, &kind)?;
    let build_marker = runtime_build_marker(&objects);
    Ok(RuntimeImage {
        executable,
        kind,
        build_marker,
        _stage: staging.finish(),
    })
}

/// The explicit first action every embedder performs.  Monitor/describe requests
/// are recognized from argv and fixed descriptors only; environment never selects
/// the mode.
pub fn earliest_bootstrap() -> io::Result<RuntimeCapability> {
    let first = std::env::args_os().nth(1);
    match first.as_deref() {
        Some(value) if value == OsStr::new(MONITOR_SENTINEL) => {
            let code = monitor_main().unwrap_or_else(|error| {
                // This is an internal bootstrap diagnostic written before normal
                // logging exists.  It contains no target environment values.
                eprintln!("sandbox monitor bootstrap failed: {error}");
                125
            });
            std::process::exit(code);
        }
        Some(value) if value == OsStr::new(DESCRIBE_SENTINEL) => {
            let code = describe_current_runtime()
                .map(|_| 0)
                .unwrap_or_else(|error| {
                    eprintln!("sandbox monitor runtime description failed: {error}");
                    125
                });
            std::process::exit(code);
        }
        Some(value) if value == OsStr::new(CANDIDATE_PROBE_SENTINEL) => {
            let code = candidate_probe_main().map(|_| 0).unwrap_or_else(|error| {
                eprintln!("sandbox candidate probe failed: {error}");
                125
            });
            std::process::exit(code);
        }
        Some(value) if value.as_bytes().starts_with(b"__nub-sandbox-") => Err(invalid_data(
            "invalid or unsupported sandbox monitor bootstrap sentinel",
        )),
        _ => Ok(RuntimeCapability::current_process()),
    }
}

fn candidate_probe_main() -> io::Result<()> {
    const PROC_SUPER_MAGIC: libc::c_long = 0x9fa0;
    const TMPFS_MAGIC: libc::c_long = 0x0102_1994;

    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(OsStr::new(CANDIDATE_PROBE_SENTINEL)) {
        return Err(invalid_data("candidate probe sentinel mismatch"));
    }
    let restricted = match arguments.next().as_deref() {
        Some(value) if value == OsStr::new("restricted") => true,
        Some(value) if value == OsStr::new("full") => false,
        _ => return Err(invalid_data("candidate probe network mode is invalid")),
    };
    let keyring_protected = match arguments.next().as_deref() {
        Some(value) if value == OsStr::new("protected") => true,
        Some(value) if value == OsStr::new("ambient") => false,
        _ => return Err(invalid_data("candidate probe keyring mode is invalid")),
    };
    // Whether this launch will itself create nested sandboxes. A nesting launch runs
    // inside an already-narrowed view, so two negative controls that assume a pristine
    // (non-inherited) view are relaxed: the /proc/keys metadata masks are intentionally
    // ABSENT (dropped so /proc stays fully visible for a nested child's procfs mount),
    // and a FULL-network target may still be unable to open a socket because the outer
    // seccomp/network ceiling it INHERITS legitimately denies it. The keyring SECCOMP
    // deny and the restricted-mode denial still hold and are still asserted.
    let nesting = match arguments.next().as_deref() {
        Some(value) if value == OsStr::new("nesting") => true,
        Some(value) if value == OsStr::new("single") => false,
        _ => return Err(invalid_data("candidate probe nesting mode is invalid")),
    };
    if arguments.next().is_some() {
        return Err(invalid_data("candidate probe received trailing arguments"));
    }
    if std::env::vars_os().next().is_some() {
        return Err(invalid_data("candidate probe environment is not empty"));
    }
    if std::env::current_dir()? != Path::new("/") {
        return Err(invalid_data(
            "candidate probe working directory is not root",
        ));
    }
    if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        return Err(invalid_data("candidate probe omitted no-new-privileges"));
    }
    verify_zero_capabilities()?;
    require_filesystem_type(Path::new("/proc"), PROC_SUPER_MAGIC, "procfs")?;
    require_filesystem_type(Path::new("/dev"), TMPFS_MAGIC, "device tmpfs")?;
    if unsafe { libc::getpid() } <= 1
        || unsafe { libc::getppid() } != 1
        || unsafe { libc::getsid(0) } != 1
    {
        return Err(invalid_data(
            "candidate probe did not run below the PID-1 monitor",
        ));
    }
    let null = fs::metadata("/dev/null")?;
    let canonical_null = null.mode() & libc::S_IFMT == libc::S_IFCHR
        && libc::major(null.rdev()) == 1
        && libc::minor(null.rdev()) == 3;
    if !canonical_null {
        match File::open("/dev/null") {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            _ => {
                return Err(invalid_data(
                    "candidate probe device tmpfs omitted the canonical null device",
                ));
            }
        }
    }
    match fs::read("/etc/ld.so.preload") {
        Ok(contents) if contents.iter().all(u8::is_ascii_whitespace) => {}
        Ok(_) => {
            return Err(invalid_data(
                "candidate probe final view exposes a nonempty /etc/ld.so.preload",
            ));
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) => {}
        Err(error) => return Err(error),
    }

    let open_socket = |family| {
        let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    };
    let unix = open_socket(libc::AF_UNIX);
    let inet = open_socket(libc::AF_INET);
    if restricted {
        for (result, family) in [(unix, "AF_UNIX"), (inet, "AF_INET")] {
            match result {
                Ok(fd) => {
                    unsafe { libc::close(fd) };
                    return Err(invalid_data(format!(
                        "candidate probe restricted target created an {family} socket"
                    )));
                }
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "candidate probe restricted {family} denial was unexpected: {error}"
                    )));
                }
            }
        }
    } else if !nesting {
        // Negative control: without a network filter a socket must open. Skipped for a
        // nesting launch, whose INHERITED outer ceiling can legitimately deny it while
        // this level itself adds no filter (outer-deny + inner-full stays denied).
        for (result, family) in [(unix, "AF_UNIX"), (inet, "AF_INET")] {
            match result {
                Ok(fd) => unsafe {
                    libc::close(fd);
                },
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "candidate probe full-network target could not create an {family} socket: {error}"
                    )));
                }
            }
        }
    } else {
        // Nesting full-network target: close any socket that DID open; do not require it.
        for fd in [unix, inet].into_iter().flatten() {
            unsafe { libc::close(fd) };
        }
    }
    if keyring_protected && !nesting {
        for path in ["/proc/keys", "/proc/key-users"] {
            match fs::read(path) {
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "candidate probe keyring procfs mask at {path} failed unexpectedly: {error}"
                    )));
                }
                Ok(_) => {
                    return Err(invalid_data(format!(
                        "candidate probe could read protected keyring state at {path}"
                    )));
                }
            }
        }
    }
    if keyring_protected {
        // keyctl option 0 is a credential op, not the anonymous session-keyring join the
        // deny filter permits, so it must still EPERM.
        require_syscall_denied("keyctl", unsafe {
            libc::syscall(libc::SYS_keyctl, 0, 0, 0, 0, 0)
        })?;
        require_syscall_denied("add_key", unsafe {
            libc::syscall(
                libc::SYS_add_key,
                std::ptr::null::<libc::c_char>(),
                std::ptr::null::<libc::c_char>(),
                std::ptr::null::<libc::c_void>(),
                0,
                0,
            )
        })?;
        require_syscall_denied("request_key", unsafe {
            libc::syscall(
                libc::SYS_request_key,
                std::ptr::null::<libc::c_char>(),
                std::ptr::null::<libc::c_char>(),
                std::ptr::null::<libc::c_char>(),
                0,
            )
        })?;
    }
    if (restricted || keyring_protected)
        && unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) } != 2
    {
        return Err(invalid_data(
            "candidate probe target omitted its required seccomp filter",
        ));
    }

    spawn_candidate_probe_tree()?;
    let name = CString::new("nub-probe-ready").expect("static probe name has no NUL");
    if unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        unsafe { libc::pause() };
    }
}

fn require_syscall_denied(name: &str, result: libc::c_long) -> io::Result<()> {
    let error = io::Error::last_os_error();
    if result == -1 && error.raw_os_error() == Some(libc::EPERM) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "candidate probe {name} denial was unexpected: result={result}, error={error}"
        )))
    }
}

fn require_filesystem_type(path: &Path, expected: libc::c_long, label: &str) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid_input("filesystem probe path contains NUL"))?;
    let mut status = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { status.assume_init() }.f_type != expected {
        return Err(invalid_data(format!(
            "candidate probe {label} has the wrong filesystem type"
        )));
    }
    Ok(())
}

fn spawn_candidate_probe_tree() -> io::Result<()> {
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        let grandchild = unsafe { libc::fork() };
        if grandchild < 0 {
            unsafe { libc::_exit(126) };
        }
        loop {
            unsafe { libc::pause() };
        }
    }

    let deadline = Instant::now() + DESCRIBE_TIMEOUT;
    loop {
        match fs::read_to_string(format!("/proc/{child}/task/{child}/children")) {
            Ok(children) if children.split_ascii_whitespace().count() == 1 => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        ensure_before_deadline(deadline, "candidate probe descendants did not become ready")?;
        thread::sleep(Duration::from_millis(2));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapSpec {
    pub(crate) session: [u8; 32],
    pub(crate) release: [u8; 32],
    pub(crate) executable: FileIdentity,
    pub(crate) build_marker: [u8; 32],
    runtime_objects: Vec<RuntimeObject>,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: BTreeMap<OsString, OsString>,
    // Whether `--unshare-net` was applied — TRUE for BOTH coarse-deny and per-host (the
    // empty netns is the boundary either way). Governs the parent's private-netns
    // attestation. DISTINCT from the socket seccomp's per-family gating, which per-host
    // narrows rather than drops (see `per_host`).
    pub(crate) network_filter: bool,
    // C3: whether the PER-HOST egress bridge is active. When set, the socket-blocking
    // seccomp STAYS installed but permits AF_INET/AF_INET6 (the families needed to reach
    // the in-netns bridge; the empty netns confines them), while `--unshare-net`/
    // `network_filter` still hold and every other family (incl. AF_UNIX) stays denied.
    pub(crate) per_host: bool,
    pub(crate) deny_keyring: bool,
    // Whether the keyring-deny seccomp permits keyctl(KEYCTL_JOIN_SESSION_KEYRING,
    // NULL) — needed ONLY by a nesting launch so a nested monitor can establish its
    // own isolation under the inherited outer filter. False for a single-level launch,
    // whose seccomp is then byte-identical to the pre-nesting filter.
    pub(crate) permit_keyring_join: bool,
    // A sealed, harness-only fault injection used to prove the monitor's
    // initial-stop deadline and cleanup path against a real child process.
    hold_before_initial_stop_for_harness: bool,
    // Preserve the cleared state-5 boundary while the harness independently
    // exercises state 6. Production construction always leaves this false.
    hold_after_exec_for_harness: bool,
    // Create a deterministic post-forward/pre-cleanup test window. This is
    // sealed bootstrap input and is always false in production construction.
    hold_before_runtime_cleanup_for_harness: bool,
    // Create a deterministic post-cleanup/pre-publication test window. This is
    // sealed bootstrap input and is always false in production construction.
    hold_after_runtime_cleanup_for_harness: bool,
    // Preserve the cleared state-6 post-TargetExited hold byte-for-byte while
    // state 7 is exercised independently. Production construction is false.
    hold_after_target_exited_for_harness: bool,
    // State-8-only integrity seam: after publishing CleanupComplete, exit with
    // a nonzero status so the parent must reject an otherwise valid protocol.
    // Production and the sealed state-5/6/7 harness modes always leave it false.
    fail_outer_status_after_cleanup_for_harness: bool,
    // Deterministically delay after accepting CompleteSession and starting its
    // deadline. Sealed harness input; production always leaves it false.
    hold_after_completion_request_for_harness: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeObject {
    path: PathBuf,
    identity: FileIdentity,
}

impl BootstrapSpec {
    pub(crate) fn new(
        runtime: &RuntimeCapability,
        policy: &SandboxPolicy,
        // C3: whether the PER-HOST egress bridge is active. Per-host confines egress with
        // the empty netns, but the target must still be able to create AF_INET/AF_INET6
        // sockets to reach the in-netns bridge on loopback — so the socket-blocking seccomp
        // filter stays installed for per-host but permits those two families (it denies
        // every family, incl. AF_UNIX, for coarse-deny). The netns is the boundary in both
        // cases.
        per_host: bool,
        spec: &CommandSpec,
        program: OsString,
        cwd: PathBuf,
        env: BTreeMap<OsString, OsString>,
    ) -> io::Result<Self> {
        let image = runtime.materialize()?;
        let mut session = [0u8; 32];
        let mut release = [0u8; 32];
        getrandom::getrandom(&mut session)
            .map_err(|error| io::Error::other(format!("generating sandbox session: {error}")))?;
        getrandom::getrandom(&mut release).map_err(|error| {
            io::Error::other(format!("generating sandbox release gate: {error}"))
        })?;
        let runtime_objects = runtime_objects(image)?;
        Ok(Self {
            session,
            release,
            executable: image.executable.identity,
            build_marker: image.build_marker,
            runtime_objects,
            program,
            // A verbatim command line has no meaning on this path: the monitor
            // `execve`s the target directly rather than handing a line to a shell that
            // parses it. Validation already refuses one off Windows, so this is the
            // belt — fail rather than exec with an empty argv.
            args: spec
                .args
                .argv()
                .ok_or_else(|| {
                    io::Error::other(
                        "a verbatim command line cannot be launched through the sandbox monitor",
                    )
                })?
                .to_vec(),
            cwd,
            env,
            network_filter: policy.net.enforce,
            per_host,
            deny_keyring: super::linux::protects_ambient_credentials(policy),
            permit_keyring_join: spec.require_nesting,
            hold_before_initial_stop_for_harness: false,
            hold_after_exec_for_harness: false,
            hold_before_runtime_cleanup_for_harness: false,
            hold_after_runtime_cleanup_for_harness: false,
            hold_after_target_exited_for_harness: false,
            fail_outer_status_after_cleanup_for_harness: false,
            hold_after_completion_request_for_harness: false,
        })
    }

    pub(crate) fn candidate_probe(
        runtime: &RuntimeCapability,
        network_filter: bool,
        deny_keyring: bool,
        nesting: bool,
    ) -> io::Result<Self> {
        let image = runtime.materialize()?;
        let (program, mut args) = target_probe_command(image, CANDIDATE_PROBE_SENTINEL);
        args.push(OsString::from(if network_filter {
            "restricted"
        } else {
            "full"
        }));
        args.push(OsString::from(if deny_keyring {
            "protected"
        } else {
            "ambient"
        }));
        // A nesting launch relaxes the inheritance-blind negative controls: the
        // /proc/keys masks are absent (so /proc stays fully visible) and a full-network
        // target may be denied a socket by the inherited outer ceiling. The keyring
        // seccomp deny and the restricted-mode denial still hold.
        args.push(OsString::from(if nesting { "nesting" } else { "single" }));
        let mut session = [0u8; 32];
        let mut release = [0u8; 32];
        getrandom::getrandom(&mut session)
            .map_err(|error| io::Error::other(format!("generating probe session: {error}")))?;
        getrandom::getrandom(&mut release)
            .map_err(|error| io::Error::other(format!("generating probe release gate: {error}")))?;
        Ok(Self {
            session,
            release,
            executable: image.executable.identity,
            build_marker: image.build_marker,
            runtime_objects: runtime_objects(image)?,
            program,
            args,
            cwd: PathBuf::from("/"),
            env: BTreeMap::new(),
            network_filter,
            per_host: false,
            deny_keyring,
            permit_keyring_join: nesting,
            hold_before_initial_stop_for_harness: false,
            hold_after_exec_for_harness: false,
            hold_before_runtime_cleanup_for_harness: false,
            hold_after_runtime_cleanup_for_harness: false,
            hold_after_target_exited_for_harness: false,
            fail_outer_status_after_cleanup_for_harness: false,
            hold_after_completion_request_for_harness: false,
        })
    }

    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        validate_bootstrap_strings(&self.program, &self.args, &self.cwd, &self.env)?;
        validate_runtime_objects(self.executable, &self.runtime_objects)?;
        validate_item_count(self.args.len())?;
        validate_item_count(self.env.len())?;
        let mut out = Vec::with_capacity(4096);
        out.extend_from_slice(BOOT_MAGIC);
        put_u16(&mut out, PROTOCOL_VERSION);
        let flags = u16::from(self.network_filter)
            | (u16::from(self.hold_before_initial_stop_for_harness) << 1)
            | (u16::from(self.hold_after_exec_for_harness) << 2)
            | (u16::from(self.hold_before_runtime_cleanup_for_harness) << 3)
            | (u16::from(self.hold_after_runtime_cleanup_for_harness) << 4)
            | (u16::from(self.hold_after_target_exited_for_harness) << 5)
            | (u16::from(self.fail_outer_status_after_cleanup_for_harness) << 6)
            | (u16::from(self.deny_keyring) << 7)
            | (u16::from(self.hold_after_completion_request_for_harness) << 8)
            | (u16::from(self.permit_keyring_join) << 9)
            | (u16::from(self.per_host) << 10);
        put_u16(&mut out, flags);
        out.extend_from_slice(&self.session);
        out.extend_from_slice(&self.release);
        put_identity(&mut out, self.executable);
        out.extend_from_slice(&self.build_marker);
        validate_item_count(self.runtime_objects.len())?;
        put_u32_checked(&mut out, self.runtime_objects.len())?;
        for object in &self.runtime_objects {
            put_bytes(&mut out, object.path.as_os_str().as_bytes())?;
            put_identity(&mut out, object.identity);
        }
        put_bytes(&mut out, self.program.as_bytes())?;
        put_vec_os(&mut out, &self.args)?;
        put_bytes(&mut out, self.cwd.as_os_str().as_bytes())?;
        put_u32_checked(&mut out, self.env.len())?;
        for (key, value) in &self.env {
            put_bytes(&mut out, key.as_bytes())?;
            put_bytes(&mut out, value.as_bytes())?;
        }
        if out.len() > MAX_BOOTSTRAP_BYTES {
            return Err(invalid_input("sandbox bootstrap exceeds the byte budget"));
        }
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_BOOTSTRAP_BYTES {
            return Err(invalid_data("sandbox bootstrap exceeds the byte budget"));
        }
        let mut cursor = Cursor::new(bytes);
        cursor.expect(BOOT_MAGIC)?;
        if cursor.u16()? != PROTOCOL_VERSION {
            return Err(invalid_data("unsupported sandbox bootstrap version"));
        }
        let flags = cursor.u16()?;
        if flags & !0b111_1111_1111 != 0 {
            return Err(invalid_data("unknown sandbox bootstrap flags"));
        }
        let session = cursor.array::<32>()?;
        let release = cursor.array::<32>()?;
        let executable = cursor.identity()?;
        let build_marker = cursor.array::<32>()?;
        let runtime_len = cursor.count()?;
        if runtime_len == 0 || runtime_len > MAX_RUNTIME_OBJECTS {
            return Err(invalid_data(
                "sandbox monitor runtime object budget is invalid",
            ));
        }
        let mut runtime_objects = Vec::with_capacity(runtime_len);
        for _ in 0..runtime_len {
            let path = PathBuf::from(OsString::from_vec(cursor.bytes()?.to_vec()));
            let identity = cursor.identity()?;
            runtime_objects.push(RuntimeObject { path, identity });
        }
        validate_runtime_objects(executable, &runtime_objects)?;
        let program = OsString::from_vec(cursor.bytes()?.to_vec());
        let args = cursor.vec_os()?;
        let cwd = PathBuf::from(OsString::from_vec(cursor.bytes()?.to_vec()));
        let env_len = cursor.count()?;
        let mut env = BTreeMap::new();
        for _ in 0..env_len {
            let key = OsString::from_vec(cursor.bytes()?.to_vec());
            let value = OsString::from_vec(cursor.bytes()?.to_vec());
            if env.insert(key, value).is_some() {
                return Err(invalid_data("duplicate sandbox environment key"));
            }
        }
        cursor.finish()?;
        validate_bootstrap_strings(&program, &args, &cwd, &env)?;
        Ok(Self {
            session,
            release,
            executable,
            build_marker,
            runtime_objects,
            program,
            args,
            cwd,
            env,
            network_filter: flags & 1 != 0,
            per_host: flags & 1024 != 0,
            deny_keyring: flags & 128 != 0,
            permit_keyring_join: flags & 512 != 0,
            hold_before_initial_stop_for_harness: flags & 2 != 0,
            hold_after_exec_for_harness: flags & 4 != 0,
            hold_before_runtime_cleanup_for_harness: flags & 8 != 0,
            hold_after_runtime_cleanup_for_harness: flags & 16 != 0,
            hold_after_target_exited_for_harness: flags & 32 != 0,
            fail_outer_status_after_cleanup_for_harness: flags & 64 != 0,
            hold_after_completion_request_for_harness: flags & 256 != 0,
        })
    }

    /// Whether the target installs a seccomp filter. Per-host STILL installs the socket
    /// filter (a per-host variant that permits only AF_INET/AF_INET6 — the families needed
    /// to reach the in-netns bridge — while keeping AF_UNIX, AF_VSOCK, and the other
    /// netns-unconfined families denied) — so `network_filter` alone gates the filter
    /// regardless of `per_host`.
    fn uses_seccomp(&self) -> bool {
        self.network_filter || self.deny_keyring
    }
}

fn validate_runtime_objects(executable: FileIdentity, objects: &[RuntimeObject]) -> io::Result<()> {
    if objects.is_empty() || objects.len() > MAX_RUNTIME_OBJECTS {
        return Err(invalid_data(
            "sandbox monitor runtime object budget is invalid",
        ));
    }
    let mut paths = BTreeSet::new();
    for object in objects {
        if !valid_private_runtime_path(&object.path) || !paths.insert(object.path.clone()) {
            return Err(invalid_data(
                "sandbox monitor runtime object path is invalid or duplicated",
            ));
        }
    }
    let private_executable = Path::new(PRIVATE_RUNTIME_ROOT).join("nub-monitor");
    if !objects
        .iter()
        .any(|object| object.path == private_executable && object.identity == executable)
    {
        return Err(invalid_data(
            "sandbox monitor runtime objects omit the executable identity",
        ));
    }
    Ok(())
}

fn valid_private_runtime_path(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(PRIVATE_RUNTIME_ROOT) else {
        return false;
    };
    let parts = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    matches!(
        parts.as_deref(),
        Some([name]) if *name == OsStr::new("nub-monitor") || *name == OsStr::new("ld.so")
    ) || matches!(
        parts.as_deref(),
        Some([directory, name])
            if *directory == OsStr::new("lib")
                && !name.as_bytes().is_empty()
                && !name.as_bytes().contains(&b':')
    )
}

fn runtime_objects(image: &RuntimeImage) -> io::Result<Vec<RuntimeObject>> {
    runtime_objects_from_parts(&image.executable, &image.kind)
}

fn runtime_objects_from_parts(
    executable: &PinnedObject,
    kind: &RuntimeKind,
) -> io::Result<Vec<RuntimeObject>> {
    let root = Path::new(PRIVATE_RUNTIME_ROOT);
    let mut objects = vec![RuntimeObject {
        path: root.join("nub-monitor"),
        identity: executable.identity,
    }];
    if let RuntimeKind::Dynamic {
        loader, libraries, ..
    } = kind
    {
        objects.push(RuntimeObject {
            path: root.join("ld.so"),
            identity: loader.identity,
        });
        for library in libraries {
            objects.push(RuntimeObject {
                path: root.join("lib").join(&library.private_name),
                identity: library.identity,
            });
        }
    }
    if objects.len() > MAX_RUNTIME_OBJECTS {
        return Err(invalid_data(
            "sandbox monitor runtime object budget exceeded",
        ));
    }
    Ok(objects)
}

/// Close Bubblewrap's option parsing before the command word: it keeps reading
/// options until `--`, so without the separator a program name beginning with `--`
/// would be consumed as another Bubblewrap option. Nub's program is always its own
/// fixed monitor, never caller-supplied, so the separator is the second defence
/// rather than the only one.
fn append_command_position(command: &mut Command, program: &OsStr, args: &[OsString]) {
    command
        .arg("--")
        .arg(program)
        .args(args)
        .arg(MONITOR_SENTINEL);
}

/// Prepared, one-shot authority for starting the retained PID-1 monitor.
/// Every child-side descriptor is relocated above the protocol's reserved range
/// before this value is returned.
pub(crate) struct RetainedMonitorLaunch {
    spec: BootstrapSpec,
    bootstrap: OwnedFd,
    control_parent: Option<OwnedFd>,
    control_child: Option<OwnedFd>,
    release_reader: Option<File>,
    release_writer: Option<File>,
    signal_reader: Option<File>,
    signal_writer: Option<File>,
    info_reader: Option<File>,
    info_writer: Option<File>,
    bindings: Vec<File>,
    runtime_objects: Vec<RuntimeObject>,
    monitor_program: OsString,
    monitor_args: Vec<OsString>,
}

impl RetainedMonitorLaunch {
    pub(crate) fn new(runtime: &RuntimeCapability, spec: BootstrapSpec) -> io::Result<Self> {
        let image = runtime.materialize()?;
        if spec.executable != image.executable.identity || spec.build_marker != image.build_marker {
            return Err(invalid_input(
                "sandbox bootstrap does not match the retained runtime image",
            ));
        }
        let runtime_objects = runtime_objects(image)?;
        if spec.runtime_objects != runtime_objects {
            return Err(invalid_input(
                "sandbox bootstrap runtime manifest does not match its bindings",
            ));
        }
        let bootstrap =
            relocate_fd_at_least(sealed_memfd(&spec.encode()?)?, FIRST_UNRESERVED_MONITOR_FD)?;
        let (control_parent, control_child) = seqpacket_pair()?;
        set_passcred(control_parent.as_raw_fd())?;
        let (release_reader, release_writer) = pipe_files(false)?;
        let (signal_reader, signal_writer) = signal_relay_pipe_files()?;
        let (info_reader, info_writer) = pipe_files(true)?;
        let bindings = runtime_bindings(image)?
            .into_iter()
            .map(|file| duplicate_file_at_least(&file, FIRST_UNRESERVED_MONITOR_FD))
            .collect::<io::Result<Vec<_>>>()?;
        if bindings.len() != runtime_objects.len() {
            return Err(invalid_data(
                "sandbox runtime binding and manifest counts differ",
            ));
        }
        let (monitor_program, monitor_args) = private_monitor_command(image);
        Ok(Self {
            spec,
            bootstrap,
            control_parent: Some(control_parent),
            control_child: Some(relocate_fd_at_least(
                control_child,
                FIRST_UNRESERVED_MONITOR_FD,
            )?),
            release_reader: Some(relocate_file_at_least(
                release_reader,
                FIRST_UNRESERVED_MONITOR_FD,
            )?),
            release_writer: Some(release_writer),
            signal_reader: Some(relocate_file_at_least(
                signal_reader,
                FIRST_UNRESERVED_MONITOR_FD,
            )?),
            signal_writer: Some(signal_writer),
            info_reader: Some(info_reader),
            info_writer: Some(relocate_file_at_least(
                info_writer,
                FIRST_UNRESERVED_MONITOR_FD,
            )?),
            bindings,
            runtime_objects,
            monitor_program,
            monitor_args,
        })
    }

    /// Relocate a caller-owned Bubblewrap setup file before its descriptor is
    /// serialized into Bubblewrap arguments.
    pub(crate) fn relocate_setup_file(file: File) -> io::Result<File> {
        relocate_file_at_least(file, FIRST_UNRESERVED_MONITOR_FD)
    }

    /// Append the private runtime mount immediately after Bubblewrap's `/dev`
    /// construction. Other filesystem grants may follow this method.
    pub(crate) fn append_runtime_mount(&self, command: &mut Command) {
        let root = Path::new(PRIVATE_RUNTIME_ROOT);
        command
            .arg("--dir")
            .arg("/dev/.nub-sandbox")
            .arg("--tmpfs")
            .arg(PRIVATE_SUPPORT_ROOT)
            .arg("--tmpfs")
            .arg(root)
            .arg("--dir")
            .arg(root.join("lib"));
        for (binding, object) in self.bindings.iter().zip(&self.runtime_objects) {
            command
                .arg("--ro-bind-fd")
                .arg(binding.as_raw_fd().to_string())
                .arg(&object.path);
        }
        command.arg("--remount-ro").arg(root);
    }

    /// Append final Bubblewrap options after all policy mounts are complete.
    /// The command itself must stay outside `--args`: stock Bubblewrap parses
    /// only options from an argument file and requires COMMAND in the outer argv.
    pub(crate) fn append_monitor_options(&self, command: &mut Command) -> io::Result<()> {
        let info = self
            .info_writer
            .as_ref()
            .ok_or_else(|| invalid_input("sandbox monitor launch was already consumed"))?;
        command
            .arg("--info-fd")
            .arg(info.as_raw_fd().to_string())
            // The retained monitor owns the namespace root and must not inherit
            // an outer cwd that happens to remain reachable through the mount
            // plan. Bubblewrap also derives its implementation-added PWD from
            // this directory after --clearenv.
            .arg("--chdir")
            .arg("/")
            .arg("--clearenv");
        Ok(())
    }

    pub(crate) fn append_monitor_command(&self, command: &mut Command) {
        append_command_position(command, &self.monitor_program, &self.monitor_args);
    }

    /// Install the only child-side descriptor hook. The whole inherited range
    /// is CLOEXEC by default; only Bubblewrap setup data and the four exact
    /// monitor protocol destinations survive exec.
    pub(crate) fn install_pre_exec(
        &self,
        command: &mut Command,
        caller_setup_fds: &[RawFd],
    ) -> io::Result<()> {
        let control_child = self
            .control_child
            .as_ref()
            .ok_or_else(|| invalid_input("sandbox monitor launch was already consumed"))?;
        let release_reader = self
            .release_reader
            .as_ref()
            .ok_or_else(|| invalid_input("sandbox monitor launch was already consumed"))?;
        let signal_reader = self
            .signal_reader
            .as_ref()
            .ok_or_else(|| invalid_input("sandbox monitor launch was already consumed"))?;
        let info_writer = self
            .info_writer
            .as_ref()
            .ok_or_else(|| invalid_input("sandbox monitor launch was already consumed"))?;
        let remaps = vec![
            (self.bootstrap.as_raw_fd(), BOOTSTRAP_FD),
            (control_child.as_raw_fd(), CONTROL_FD),
            (release_reader.as_raw_fd(), RELEASE_FD),
            (signal_reader.as_raw_fd(), SIGNAL_RELAY_FD),
        ];
        let inherited = self
            .bindings
            .iter()
            .map(AsRawFd::as_raw_fd)
            .chain(std::iter::once(info_writer.as_raw_fd()))
            .chain(caller_setup_fds.iter().copied())
            .collect::<BTreeSet<_>>();
        let deny_keyring = self.spec.deny_keyring;
        if inherited.iter().any(|fd| *fd < FIRST_UNRESERVED_MONITOR_FD) {
            return Err(invalid_input(
                "Bubblewrap setup descriptor overlaps the monitor protocol range",
            ));
        }
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if deny_keyring {
                    join_anonymous_session_keyring()?;
                }
                mark_inherited_fds_cloexec()?;
                for &(source, destination) in &remaps {
                    if source == destination {
                        clear_cloexec(destination)?;
                    } else if libc::dup2(source, destination) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                for &fd in &inherited {
                    clear_cloexec(fd)?;
                }
                Ok(())
            });
        }
        Ok(())
    }

    pub(crate) fn start(self, outer: Child) -> io::Result<(Child, RetainedMonitorSession)> {
        self.start_until(outer, None)
    }

    pub(crate) fn run_candidate_probe(self, outer: Child, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let (mut outer, mut session) = self.start_until(outer, Some(deadline))?;
        session.wait_probe_until(&mut outer, deadline)
    }

    fn start_until(
        mut self,
        mut outer: Child,
        deadline: Option<Instant>,
    ) -> io::Result<(Child, RetainedMonitorSession)> {
        drop(self.control_child.take());
        drop(self.release_reader.take());
        drop(self.signal_reader.take());
        drop(self.info_writer.take());
        let result = (|| {
            let mut info = self
                .info_reader
                .take()
                .ok_or_else(|| invalid_input("sandbox monitor info channel is absent"))?;
            let monitor_pid = match deadline {
                Some(deadline) => read_bwrap_child_pid_until(&mut info, &mut outer, deadline)?,
                None => read_bwrap_child_pid(&mut info, &mut outer)?,
            };
            let control_parent = self
                .control_parent
                .take()
                .ok_or_else(|| invalid_input("sandbox monitor control channel is absent"))?;
            set_control_timeouts(control_parent.as_raw_fd(), DESCRIBE_TIMEOUT)?;
            let mut control = ControlChannel::new(
                control_parent,
                self.spec.session,
                ExpectedPeer {
                    pid: Some(monitor_pid),
                    uid: unsafe { libc::getuid() },
                },
            )?;
            let ready = receive_launch_frame(&mut control, deadline)?;
            require_monitor_ready(&ready, self.spec.executable, self.spec.build_marker)?;
            verify_monitor_host_state(monitor_pid, outer.id())?;
            verify_production_monitor_namespaces(monitor_pid, self.spec.network_filter)?;

            let mut release = self
                .release_writer
                .take()
                .ok_or_else(|| invalid_input("sandbox monitor release capability is absent"))?;
            if let Some(deadline) = deadline {
                ensure_before_deadline(deadline, "candidate probe release deadline elapsed")?;
            }
            release.write_all(&self.spec.release)?;
            drop(release);
            let stopped = receive_launch_frame(&mut control, deadline)?;
            if stopped.kind != FrameKind::TargetStopped {
                return Err(unexpected_monitor_frame("TargetStopped", &stopped));
            }
            let attestation = TargetStoppedAttestation::decode(&stopped.payload)?;
            let target_pid = read_unique_monitor_child(monitor_pid)?;
            verify_target_host_state(
                target_pid,
                monitor_pid,
                attestation,
                ExpectedTargetState::Stopped,
                self.spec.uses_seccomp(),
            )?;
            control.send_with_deadline(FrameKind::StartTarget, attestation.encode(), deadline)?;
            let accepted = receive_launch_frame(&mut control, deadline)?;
            match accepted.kind {
                FrameKind::ExecAccepted if accepted.payload.is_empty() => {}
                FrameKind::ExecFailed => {
                    let failure = TargetExecFailure::decode(&accepted.payload)?;
                    return Err(io::Error::other(format!(
                        "sandbox target exec failed: {}",
                        io::Error::from_raw_os_error(failure.errno)
                    )));
                }
                _ => return Err(unexpected_monitor_frame("ExecAccepted", &accepted)),
            }
            set_control_timeouts(control.fd.as_raw_fd(), Duration::ZERO)?;
            let signal = SignalRelay::new(
                self.signal_writer
                    .take()
                    .ok_or_else(|| invalid_input("sandbox signal relay is absent"))?,
            );
            Ok(RetainedMonitorSession {
                control,
                signal,
                target_pid,
                authenticated_cleanup: false,
                outer_reaped: false,
            })
        })();
        match result {
            Ok(session) => Ok((outer, session)),
            Err(error) => {
                // Close every surviving launch capability before terminating the
                // exact outer process. This makes EOF part of the fail-closed path
                // instead of relying on the launch value's eventual field drops.
                drop(self.control_parent.take());
                drop(self.control_child.take());
                drop(self.release_reader.take());
                drop(self.release_writer.take());
                drop(self.signal_reader.take());
                drop(self.signal_writer.take());
                drop(self.info_reader.take());
                drop(self.info_writer.take());
                match fail_closed_outer(&mut outer) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(io::Error::new(
                        error.kind(),
                        format!("{error}; Bubblewrap cleanup also failed: {cleanup}"),
                    )),
                }
            }
        }
    }
}

fn join_anonymous_session_keyring() -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            libc::c_long::from(libc::KEYCTL_JOIN_SESSION_KEYRING),
            std::ptr::null::<libc::c_char>(),
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct SignalRelay {
    state: Arc<Mutex<SignalRelayState>>,
}

struct SignalRelayState {
    writer: Option<File>,
    sequence: u64,
    poisoned: Option<String>,
    disarmed: bool,
}

pub(crate) type SignalRelayCallback = super::PreparedSignalCallback;

impl SignalRelay {
    fn new(writer: File) -> Self {
        Self {
            state: Arc::new(Mutex::new(SignalRelayState {
                writer: Some(writer),
                sequence: 0,
                poisoned: None,
                disarmed: false,
            })),
        }
    }

    pub(crate) fn callback(&self) -> SignalRelayCallback {
        let relay = self.clone();
        Arc::new(move |signal| relay.forward(signal))
    }

    pub(crate) fn forward(&self, signal: libc::c_int) -> io::Result<()> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.writer.take();
                let message = "sandbox signal relay failed permanently: relay lock poisoned";
                state.poisoned = Some(message.to_string());
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, message));
            }
        };
        if state.disarmed {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "sandbox signal relay is retired",
            ));
        }
        if let Some(error) = &state.poisoned {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
        }
        let next = match state.sequence.checked_add(1) {
            Some(next) => next,
            None => return poison_signal_relay(&mut state, "signal sequence overflow"),
        };
        let packet = match (SignalRelayRecord {
            sequence: state.sequence,
            signal,
        })
        .encode()
        {
            Ok(packet) => packet,
            Err(error) => return poison_signal_relay(&mut state, &error.to_string()),
        };
        let Some(writer) = state.writer.as_ref() else {
            return poison_signal_relay(&mut state, "signal writer is closed");
        };
        let written =
            unsafe { libc::write(writer.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
        if written != packet.len() as isize {
            let detail = if written < 0 {
                io::Error::last_os_error().to_string()
            } else {
                "partial packet write".to_string()
            };
            return poison_signal_relay(&mut state, &detail);
        }
        state.sequence = next;
        Ok(())
    }

    fn disarm(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.disarmed = true;
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.disarmed = true;
        state.writer.take();
    }

    /// Whether the relay still owns the write end. Tests assert the retain/release
    /// contract on this owned state rather than on the reader observing pipe EOF:
    /// `fork` copies the whole descriptor table and `O_CLOEXEC` does not apply to it,
    /// so any sibling test forking concurrently (cargo runs the suite multi-threaded)
    /// transiently holds a second write end and suppresses the EOF this process would
    /// otherwise see. Pipe EOF is a process-global property; relay ownership is not.
    #[cfg(test)]
    fn writer_retained(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .writer
            .is_some()
    }
}

fn poison_signal_relay(state: &mut SignalRelayState, detail: &str) -> io::Result<()> {
    let message = format!("sandbox signal relay failed permanently: {detail}");
    state.writer.take();
    state.poisoned = Some(message.clone());
    Err(io::Error::new(io::ErrorKind::BrokenPipe, message))
}

pub(crate) struct RetainedMonitorSession {
    control: ControlChannel,
    signal: SignalRelay,
    target_pid: libc::pid_t,
    authenticated_cleanup: bool,
    outer_reaped: bool,
}

impl RetainedMonitorSession {
    pub(crate) fn target_pid(&self) -> libc::pid_t {
        self.target_pid
    }

    pub(crate) fn signal_callback(&self) -> SignalRelayCallback {
        self.signal.callback()
    }

    pub(crate) fn cleanup_complete(&self) -> bool {
        self.outer_reaped
    }

    pub(crate) fn wait(&mut self, outer: &mut Child) -> io::Result<ExitStatus> {
        let outcome = (|| {
            let attestation = self.receive_target_exited(outer, None)?;
            self.signal.disarm();
            let deadline = Instant::now() + DESCRIBE_TIMEOUT;
            let result = self.complete(outer, attestation, deadline)?;
            Ok(result.status)
        })();
        self.finish_or_fail_closed(outer, outcome)
    }

    fn wait_probe_until(&mut self, outer: &mut Child, deadline: Instant) -> io::Result<()> {
        let outcome = (|| {
            self.wait_for_candidate_probe_ready(outer, deadline)?;
            self.signal.forward(libc::SIGTERM)?;
            let attestation = self.receive_target_exited(outer, Some(deadline))?;
            self.signal.disarm();
            ExpectedTargetExit::Signaled(libc::SIGTERM).verify(attestation.report.raw_status)?;
            if attestation.report.descendants_reaped != 2 {
                return Err(invalid_data(format!(
                    "candidate probe reported {} descendant reaps, expected 2",
                    attestation.report.descendants_reaped
                )));
            }
            let completion_deadline = deadline.min(Instant::now() + DESCRIBE_TIMEOUT);
            self.complete(outer, attestation, completion_deadline)?;
            Ok(())
        })();
        self.finish_or_fail_closed(outer, outcome)
    }

    pub(crate) fn fail_closed(&mut self, outer: &mut Child) -> io::Result<()> {
        self.signal.close();
        let _ = unsafe { libc::shutdown(self.control.fd.as_raw_fd(), libc::SHUT_RDWR) };
        let result = fail_closed_outer(outer);
        if result.is_ok() {
            self.outer_reaped = true;
        }
        result
    }

    fn finish_or_fail_closed<T>(
        &mut self,
        outer: &mut Child,
        result: io::Result<T>,
    ) -> io::Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) if self.outer_reaped => Err(error),
            Err(error) => match self.fail_closed(outer) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "{error}; retained-monitor fail-closed cleanup failed: {cleanup}"
                ))),
            },
        }
    }

    fn receive_target_exited(
        &mut self,
        outer: &mut Child,
        deadline: Option<Instant>,
    ) -> io::Result<CompletionAttestation> {
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "candidate probe session deadline elapsed",
                ));
            }
            if control_packet_available(self.control.fd.as_raw_fd())? {
                let frame = match deadline {
                    Some(deadline) => self.control.receive_with_deadline(Some(deadline))?,
                    None => self.control.receive()?,
                };
                if frame.kind != FrameKind::TargetExited {
                    return Err(unexpected_monitor_frame("TargetExited", &frame));
                }
                return CompletionAttestation::decode(&frame.payload);
            }
            if let Some(status) = outer.try_wait()? {
                return Err(invalid_data(format!(
                    "Bubblewrap exited before authenticated target completion: {status}"
                )));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for_candidate_probe_ready(
        &mut self,
        outer: &mut Child,
        deadline: Instant,
    ) -> io::Result<()> {
        loop {
            ensure_before_deadline(deadline, "candidate probe readiness deadline elapsed")?;
            if control_packet_available(self.control.fd.as_raw_fd())? {
                let frame = self.control.receive_with_deadline(Some(deadline))?;
                return Err(unexpected_monitor_frame("probe readiness", &frame));
            }
            if let Some(status) = outer.try_wait()? {
                return Err(invalid_data(format!(
                    "Bubblewrap exited before candidate probe readiness: {status}"
                )));
            }
            match fs::read_to_string(format!("/proc/{}/comm", self.target_pid)) {
                Ok(name) if name.trim_end() == "nub-probe-ready" => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn complete(
        &mut self,
        outer: &mut Child,
        original: CompletionAttestation,
        deadline: Instant,
    ) -> io::Result<ParentCompletionResult> {
        self.control.send_with_deadline(
            FrameKind::CompleteSession,
            original.encode()?,
            Some(deadline),
        )?;
        shutdown_control_write(&self.control)?;
        let cleanup = receive_control_until(&mut self.control, deadline)?;
        if cleanup.kind != FrameKind::CleanupComplete {
            return Err(unexpected_monitor_frame("CleanupComplete", &cleanup));
        }
        original.require_exact_echo(&cleanup.payload)?;
        self.authenticated_cleanup = true;
        match receive_control_until(&mut self.control, deadline) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
            Err(error) => return Err(error),
            Ok(frame) => {
                return Err(invalid_data(format!(
                    "received unexpected post-completion {:?} frame",
                    frame.kind
                )));
            }
        }
        let outer_status = wait_for_child_status_until(outer, deadline)?;
        self.outer_reaped = true;
        self.signal.close();
        debug_assert!(self.authenticated_cleanup);
        if outer_status.code() != Some(0) {
            return Err(invalid_data(format!(
                "retained-monitor outer process exited with {outer_status}, expected code 0"
            )));
        }
        ensure_before_deadline(deadline, "retained-monitor completion deadline elapsed")?;
        Ok(ParentCompletionResult {
            status: ExitStatus::from_raw(original.report.raw_status),
            report: original.report,
        })
    }
}

fn receive_launch_frame(
    control: &mut ControlChannel,
    deadline: Option<Instant>,
) -> io::Result<Frame> {
    match deadline {
        Some(deadline) => receive_control_until(control, deadline),
        None => control.receive(),
    }
}

impl Drop for RetainedMonitorSession {
    fn drop(&mut self) {
        if !self.outer_reaped {
            self.signal.close();
            let _ = unsafe { libc::shutdown(self.control.fd.as_raw_fd(), libc::SHUT_RDWR) };
        }
    }
}

fn fail_closed_outer(outer: &mut Child) -> io::Result<()> {
    // Closing the authenticated control endpoint asks the PID-1 monitor to kill
    // and reap its namespace before Bubblewrap exits. Give that exact child a
    // short bounded chance to finish; killing Bubblewrap immediately can orphan
    // the namespace target before PID 1 observes the control EOF.
    let graceful_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match outer.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < graceful_deadline => {
                thread::sleep(Duration::from_millis(2));
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    match outer.kill() {
        Ok(()) => {}
        Err(error)
            if error.kind() == io::ErrorKind::InvalidInput
                || error.raw_os_error() == Some(libc::ESRCH) => {}
        Err(error) => return Err(error),
    }
    loop {
        match outer.wait() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn require_monitor_ready(
    ready: &Frame,
    executable: FileIdentity,
    build_marker: [u8; 32],
) -> io::Result<()> {
    if ready.kind != FrameKind::MonitorReady || ready.payload.len() != 56 {
        return Err(unexpected_monitor_frame("MonitorReady", ready));
    }
    let mut cursor = Cursor::new(&ready.payload);
    if cursor.identity()? != executable || cursor.array::<32>()? != build_marker {
        return Err(invalid_data("monitor ready attestation identity mismatch"));
    }
    cursor.finish()
}

fn unexpected_monitor_frame(expected: &str, frame: &Frame) -> io::Error {
    if frame.kind == FrameKind::Fatal {
        invalid_data(format!(
            "sandbox monitor reported fatal failure while awaiting {expected}: {}",
            String::from_utf8_lossy(&frame.payload)
        ))
    } else {
        invalid_data(format!(
            "expected sandbox monitor {expected}, received {:?}",
            frame.kind
        ))
    }
}

fn verify_production_monitor_namespaces(
    monitor_pid: libc::pid_t,
    require_private_network: bool,
) -> io::Result<()> {
    for namespace in ["user", "mnt", "pid", "ipc", "uts"] {
        let parent = fs::read_link(format!("/proc/self/ns/{namespace}"))?;
        let monitor = fs::read_link(format!("/proc/{monitor_pid}/ns/{namespace}"))?;
        if monitor == parent {
            return Err(invalid_data(format!(
                "retained monitor did not enter a distinct {namespace} namespace"
            )));
        }
    }
    let parent_network = fs::read_link("/proc/self/ns/net")?;
    let monitor_network = fs::read_link(format!("/proc/{monitor_pid}/ns/net"))?;
    if require_private_network {
        if monitor_network == parent_network {
            return Err(invalid_data(
                "retained monitor did not enter the required private network namespace",
            ));
        }
    } else if monitor_network != parent_network {
        return Err(invalid_data(
            "retained monitor unexpectedly changed the full-network namespace",
        ));
    }
    Ok(())
}

fn private_monitor_command(image: &RuntimeImage) -> (OsString, Vec<OsString>) {
    let root = Path::new(PRIVATE_RUNTIME_ROOT);
    match &image.kind {
        RuntimeKind::Static => (root.join("nub-monitor").into_os_string(), Vec::new()),
        RuntimeKind::Dynamic {
            family,
            inhibit_rpath,
            ..
        } => {
            let mut args = Vec::new();
            if *family == LoaderFamily::Glibc {
                args.push(OsString::from("--inhibit-cache"));
            }
            args.push(OsString::from("--library-path"));
            args.push(root.join("lib").into_os_string());
            if *family == LoaderFamily::Glibc {
                args.push(OsString::from("--inhibit-rpath"));
                args.push(inhibit_rpath.clone());
            }
            args.push(root.join("nub-monitor").into_os_string());
            (root.join("ld.so").into_os_string(), args)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum FrameKind {
    MonitorReady = 1,
    StartTarget = 2,
    TargetStopped = 3,
    ExecAccepted = 4,
    ExecFailed = 5,
    Terminate = 6,
    CleanupComplete = 7,
    Fatal = 8,
    TargetExited = 9,
    CompleteSession = 10,
}

impl TryFrom<u16> for FrameKind {
    type Error = io::Error;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::MonitorReady),
            2 => Ok(Self::StartTarget),
            3 => Ok(Self::TargetStopped),
            4 => Ok(Self::ExecAccepted),
            5 => Ok(Self::ExecFailed),
            6 => Ok(Self::Terminate),
            7 => Ok(Self::CleanupComplete),
            8 => Ok(Self::Fatal),
            9 => Ok(Self::TargetExited),
            10 => Ok(Self::CompleteSession),
            _ => Err(invalid_data("unknown sandbox monitor frame kind")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) session: [u8; 32],
    pub(crate) sequence: u64,
    pub(crate) kind: FrameKind,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    const HEADER_LEN: usize = 8 + 2 + 2 + 8 + 4 + 32;

    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        if self.payload.len() > MAX_FRAME_PAYLOAD {
            return Err(invalid_input(
                "sandbox monitor frame exceeds payload budget",
            ));
        }
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.payload.len());
        out.extend_from_slice(FRAME_MAGIC);
        put_u16(&mut out, PROTOCOL_VERSION);
        put_u16(&mut out, self.kind as u16);
        put_u64(&mut out, self.sequence);
        put_u32_checked(&mut out, self.payload.len())?;
        out.extend_from_slice(&self.session);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8], expected_session: &[u8; 32]) -> io::Result<Self> {
        if bytes.len() < Self::HEADER_LEN || bytes.len() > Self::HEADER_LEN + MAX_FRAME_PAYLOAD {
            return Err(invalid_data("invalid sandbox monitor frame length"));
        }
        let mut cursor = Cursor::new(bytes);
        cursor.expect(FRAME_MAGIC)?;
        if cursor.u16()? != PROTOCOL_VERSION {
            return Err(invalid_data("unsupported sandbox monitor frame version"));
        }
        let kind = FrameKind::try_from(cursor.u16()?)?;
        let sequence = cursor.u64()?;
        let payload_len = cursor.u32()? as usize;
        let session = cursor.array::<32>()?;
        if !constant_time_eq(&session, expected_session) {
            return Err(invalid_data("sandbox monitor frame session mismatch"));
        }
        if cursor.remaining() != payload_len {
            return Err(invalid_data(
                "sandbox monitor frame payload length mismatch",
            ));
        }
        let payload = cursor.take(payload_len)?.to_vec();
        cursor.finish()?;
        Ok(Self {
            session,
            sequence,
            kind,
            payload,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpectedPeer {
    pid: Option<libc::pid_t>,
    uid: libc::uid_t,
}

struct ControlChannel {
    fd: OwnedFd,
    session: [u8; 32],
    send_sequence: u64,
    receive_sequence: u64,
    expected_peer: ExpectedPeer,
}

impl ControlChannel {
    fn new(fd: OwnedFd, session: [u8; 32], expected_peer: ExpectedPeer) -> io::Result<Self> {
        validate_seqpacket_socket(fd.as_raw_fd())?;
        set_passcred(fd.as_raw_fd())?;
        Ok(Self {
            fd,
            session,
            send_sequence: 0,
            receive_sequence: 0,
            expected_peer,
        })
    }

    fn send(&mut self, kind: FrameKind, payload: Vec<u8>) -> io::Result<()> {
        self.send_with_deadline(kind, payload, None)
    }

    fn send_with_deadline(
        &mut self,
        kind: FrameKind,
        payload: Vec<u8>,
        deadline: Option<Instant>,
    ) -> io::Result<()> {
        let bytes = Frame {
            session: self.session,
            sequence: self.send_sequence,
            kind,
            payload,
        }
        .encode()?;
        let next_sequence = self
            .send_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("sandbox monitor send sequence overflow"))?;
        match deadline {
            Some(deadline) => loop {
                ensure_before_deadline(deadline, "sandbox monitor control send deadline elapsed")?;
                let written = unsafe {
                    libc::send(
                        self.fd.as_raw_fd(),
                        bytes.as_ptr().cast(),
                        bytes.len(),
                        libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                    )
                };
                if written >= 0 {
                    if written as usize != bytes.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "short sandbox monitor control packet",
                        ));
                    }
                    // SOCK_SEQPACKET publication is irreversible. Once the exact
                    // packet is accepted, commit its sequence and report success;
                    // returning a timeout here would make a subsequent Fatal a
                    // replay after an already-visible result.
                    self.send_sequence = next_sequence;
                    return Ok(());
                }
                let error = io::Error::last_os_error();
                ensure_before_deadline(deadline, "sandbox monitor control send deadline elapsed")?;
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(error);
                }
                poll_control_writable_until(self.fd.as_raw_fd(), deadline)?;
            },
            None => {
                let written = loop {
                    let written = unsafe {
                        libc::send(
                            self.fd.as_raw_fd(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                            libc::MSG_NOSIGNAL,
                        )
                    };
                    if written >= 0 {
                        break written;
                    }
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                };
                if written as usize != bytes.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "short sandbox monitor control packet",
                    ));
                }
                self.send_sequence = next_sequence;
                Ok(())
            }
        }
    }

    fn receive(&mut self) -> io::Result<Frame> {
        self.receive_with_deadline(None)
    }

    fn receive_with_deadline(&mut self, deadline: Option<Instant>) -> io::Result<Frame> {
        let mut bytes = vec![0u8; Frame::HEADER_LEN + MAX_FRAME_PAYLOAD];
        let control_len =
            unsafe { libc::CMSG_SPACE(mem::size_of::<libc::ucred>() as u32) } as usize;
        let mut ancillary = aligned_ancillary(control_len);
        let mut iovec = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message = unsafe { MaybeUninit::<libc::msghdr>::zeroed().assume_init() };
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = ancillary.len() * mem::size_of::<usize>();
        let received = loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "sandbox monitor control deadline elapsed",
                ));
            }
            let result =
                unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
            if result >= 0 {
                break result as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "sandbox monitor control deadline elapsed",
                ));
            }
        };
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sandbox monitor control deadline elapsed",
            ));
        }
        if received == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sandbox monitor control channel closed",
            ));
        }
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(invalid_data("truncated sandbox monitor control packet"));
        }
        let credential = parse_single_credential(&message)?;
        if credential.uid != self.expected_peer.uid
            || self
                .expected_peer
                .pid
                .is_some_and(|expected| credential.pid != expected)
        {
            return Err(invalid_data("sandbox monitor control credential mismatch"));
        }
        let frame = Frame::decode(&bytes[..received], &self.session)?;
        if frame.sequence != self.receive_sequence {
            return Err(invalid_data(
                "sandbox monitor control sequence mismatch or replay",
            ));
        }
        self.receive_sequence = self
            .receive_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("sandbox monitor receive sequence overflow"))?;
        Ok(frame)
    }
}

fn poll_control_writable_until(fd: RawFd, deadline: Instant) -> io::Result<()> {
    loop {
        ensure_before_deadline(deadline, "sandbox monitor control send deadline elapsed")?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining
            .as_nanos()
            .saturating_add(999_999)
            .div_euclid(1_000_000)
            .clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLOUT | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        ensure_before_deadline(deadline, "sandbox monitor control send deadline elapsed")?;
        if polled > 0 {
            return Ok(());
        }
    }
}

fn aligned_ancillary(byte_len: usize) -> Vec<usize> {
    vec![0usize; byte_len.div_ceil(mem::size_of::<usize>())]
}

fn validate_seqpacket_socket(fd: RawFd) -> io::Result<()> {
    validate_fd_kind(fd, libc::S_IFSOCK, "control channel is not a socket")?;
    let mut socket_type = 0;
    let mut length = mem::size_of_val(&socket_type) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
        || socket_type != libc::SOCK_SEQPACKET
    {
        return Err(invalid_data(
            "sandbox monitor control channel is not SOCK_SEQPACKET",
        ));
    }
    let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { libc::getpeername(fd, peer.as_mut_ptr().cast(), &mut peer_len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let peer = unsafe { peer.assume_init() };
    if peer.ss_family as libc::c_int != libc::AF_UNIX {
        return Err(invalid_data(
            "sandbox monitor control channel peer is not AF_UNIX",
        ));
    }
    Ok(())
}

fn validate_raw_control_fd(fd: RawFd, expected_uid: libc::uid_t) -> io::Result<()> {
    let stat = fstat(fd)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK || stat.st_uid != expected_uid {
        return Err(invalid_data(
            "sandbox monitor control descriptor kind or ownership is invalid",
        ));
    }
    validate_seqpacket_socket(fd)?;
    let mut peer = MaybeUninit::<libc::ucred>::uninit();
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            peer.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
        || length as usize != mem::size_of::<libc::ucred>()
        || unsafe { peer.assume_init() }.uid != expected_uid
    {
        return Err(invalid_data(
            "sandbox monitor control peer ownership is invalid",
        ));
    }
    Ok(())
}

fn set_passcred(fd: RawFd) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    if unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&enabled as *const libc::c_int).cast(),
            mem::size_of_val(&enabled) as libc::socklen_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn parse_single_credential(message: &libc::msghdr) -> io::Result<libc::ucred> {
    let mut credential = None;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
    while !header.is_null() {
        let is_credential = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SCM_CREDENTIALS
                && (*header).cmsg_len
                    == libc::CMSG_LEN(mem::size_of::<libc::ucred>() as u32) as usize
        };
        if !is_credential || credential.is_some() {
            return Err(invalid_data(
                "unexpected sandbox monitor control ancillary data",
            ));
        }
        credential =
            Some(unsafe { (libc::CMSG_DATA(header).cast::<libc::ucred>()).read_unaligned() });
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
    }
    credential.ok_or_else(|| invalid_data("sandbox monitor control credential missing"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedElf {
    interpreter: Option<Vec<u8>>,
    has_dynamic: bool,
    needed: Vec<OsString>,
    soname: Option<OsString>,
    has_injection_tags: bool,
}

#[derive(Debug)]
struct InventoryObject {
    file: File,
    path: PathBuf,
    aliases: BTreeSet<OsString>,
    identity: FileIdentity,
    parsed: ParsedElf,
}

fn parse_elf_interpreter(file: &File) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0)?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err(invalid_data(
            "sandbox monitor requires a little-endian ELF64 executable",
        ));
    }
    let machine = le_u16(&header[18..20]);
    if machine != 62 && machine != 183 {
        return Err(invalid_data("unsupported sandbox monitor ELF architecture"));
    }
    let phoff = le_u64(&header[32..40]);
    let phentsize = le_u16(&header[54..56]) as u64;
    let phnum = le_u16(&header[56..58]) as usize;
    if phentsize < 56 || phnum == 0 || phnum > 1024 {
        return Err(invalid_data("invalid sandbox monitor ELF program headers"));
    }
    let mut interpreter = None;
    for index in 0..phnum {
        let offset = phoff
            .checked_add((index as u64).checked_mul(phentsize).ok_or_else(|| {
                invalid_data("sandbox monitor ELF program-header offset overflow")
            })?)
            .ok_or_else(|| invalid_data("sandbox monitor ELF program-header offset overflow"))?;
        let mut ph = [0u8; 56];
        file.read_exact_at(&mut ph, offset)?;
        if le_u32(&ph[0..4]) != 3 {
            continue;
        }
        let file_offset = le_u64(&ph[8..16]);
        let file_size = le_u64(&ph[32..40]);
        if interpreter.is_some() || !(2..=4096).contains(&file_size) {
            return Err(invalid_data("invalid sandbox monitor ELF PT_INTERP"));
        }
        let mut bytes = vec![0u8; file_size as usize];
        file.read_exact_at(&mut bytes, file_offset)?;
        if bytes.last() != Some(&0) || bytes[..bytes.len() - 1].contains(&0) {
            return Err(invalid_data("malformed sandbox monitor ELF PT_INTERP"));
        }
        bytes.pop();
        interpreter = Some(bytes);
    }
    Ok(interpreter)
}

fn parse_elf(file: &File) -> io::Result<ParsedElf> {
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0)?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err(invalid_data(
            "sandbox monitor requires a little-endian ELF64 executable",
        ));
    }
    let machine = le_u16(&header[18..20]);
    if machine != 62 && machine != 183 {
        return Err(invalid_data("unsupported sandbox monitor ELF architecture"));
    }
    let phoff = le_u64(&header[32..40]);
    let phentsize = le_u16(&header[54..56]) as u64;
    let phnum = le_u16(&header[56..58]) as usize;
    if phentsize < 56 || phnum == 0 || phnum > 1024 {
        return Err(invalid_data("invalid sandbox monitor ELF program headers"));
    }
    let mut loads = Vec::new();
    let mut interpreter = None;
    let mut dynamic = None;
    for index in 0..phnum {
        let offset = phoff
            .checked_add((index as u64).checked_mul(phentsize).ok_or_else(|| {
                invalid_data("sandbox monitor ELF program-header offset overflow")
            })?)
            .ok_or_else(|| invalid_data("sandbox monitor ELF program-header offset overflow"))?;
        let mut ph = [0u8; 56];
        file.read_exact_at(&mut ph, offset)?;
        let kind = le_u32(&ph[0..4]);
        let file_offset = le_u64(&ph[8..16]);
        let vaddr = le_u64(&ph[16..24]);
        let file_size = le_u64(&ph[32..40]);
        let mem_size = le_u64(&ph[40..48]);
        match kind {
            1 => loads.push((vaddr, mem_size, file_offset, file_size)),
            2 => {
                if dynamic.is_some() {
                    return Err(invalid_data("duplicate sandbox monitor ELF PT_DYNAMIC"));
                }
                dynamic = Some((file_offset, file_size));
            }
            3 => {
                if interpreter.is_some() || !(2..=4096).contains(&file_size) {
                    return Err(invalid_data("invalid sandbox monitor ELF PT_INTERP"));
                }
                let mut bytes = vec![0u8; file_size as usize];
                file.read_exact_at(&mut bytes, file_offset)?;
                if bytes.last() != Some(&0) || bytes[..bytes.len() - 1].contains(&0) {
                    return Err(invalid_data("malformed sandbox monitor ELF PT_INTERP"));
                }
                bytes.pop();
                interpreter = Some(bytes);
            }
            _ => {}
        }
    }

    let Some((dynamic_offset, dynamic_size)) = dynamic else {
        return Ok(ParsedElf {
            interpreter,
            has_dynamic: false,
            needed: Vec::new(),
            soname: None,
            has_injection_tags: false,
        });
    };
    if dynamic_size % 16 != 0 || dynamic_size > 1024 * 1024 {
        return Err(invalid_data("invalid sandbox monitor ELF dynamic table"));
    }
    let mut needed_offsets = Vec::new();
    let mut soname_offset = None;
    let mut inhibit_offsets = Vec::new();
    let mut strtab_vaddr = None;
    let mut strtab_size = None;
    let mut has_injection_tags = false;
    let mut terminated = false;
    for index in 0..(dynamic_size / 16) {
        let mut entry = [0u8; 16];
        file.read_exact_at(&mut entry, dynamic_offset + index * 16)?;
        let tag = le_i64(&entry[0..8]);
        let value = le_u64(&entry[8..16]);
        match tag {
            0 => {
                terminated = true;
                break;
            }
            1 => needed_offsets.push(value),
            5 => set_once(&mut strtab_vaddr, value, "duplicate ELF DT_STRTAB")?,
            10 => set_once(&mut strtab_size, value, "duplicate ELF DT_STRSZ")?,
            14 => set_once(&mut soname_offset, value, "duplicate ELF DT_SONAME")?,
            15 | 29 => inhibit_offsets.push(value),
            0x6ffffefb | 0x6ffffefc | 0x7ffffffd | 0x7fffffff => {
                has_injection_tags = true;
            }
            _ => {}
        }
    }
    if !terminated {
        return Err(invalid_data(
            "unterminated sandbox monitor ELF dynamic table",
        ));
    }
    let strtab_vaddr =
        strtab_vaddr.ok_or_else(|| invalid_data("ELF dynamic string table missing"))?;
    let strtab_size = strtab_size.ok_or_else(|| invalid_data("ELF dynamic string size missing"))?;
    if strtab_size == 0 || strtab_size > 16 * 1024 * 1024 {
        return Err(invalid_data("invalid ELF dynamic string table size"));
    }
    let strtab_offset = vaddr_to_offset(strtab_vaddr, strtab_size, &loads)?;
    let mut strings = vec![0u8; strtab_size as usize];
    file.read_exact_at(&mut strings, strtab_offset)?;
    let get = |offset: u64| -> io::Result<OsString> {
        let offset = usize::try_from(offset)
            .map_err(|_| invalid_data("ELF dynamic string offset overflow"))?;
        if offset >= strings.len() {
            return Err(invalid_data("ELF dynamic string offset out of bounds"));
        }
        let tail = &strings[offset..];
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| invalid_data("unterminated ELF dynamic string"))?;
        Ok(OsString::from_vec(tail[..end].to_vec()))
    };
    let mut needed = Vec::with_capacity(needed_offsets.len());
    for offset in needed_offsets {
        let value = get(offset)?;
        validate_loader_object_name(&value, "ELF DT_NEEDED")?;
        needed.push(value);
    }
    let soname = soname_offset.map(get).transpose()?;
    if let Some(soname) = &soname {
        validate_loader_object_name(soname, "ELF DT_SONAME")?;
    }
    for offset in inhibit_offsets {
        // The value itself is not trusted for resolution. Parsing it proves the
        // tag is structurally valid; launch inhibits search paths by object name.
        let _ = get(offset)?;
    }
    Ok(ParsedElf {
        interpreter,
        has_dynamic: true,
        needed,
        soname,
        has_injection_tags,
    })
}

fn validate_loader_object_name(value: &OsStr, label: &str) -> io::Result<()> {
    if value.as_bytes().is_empty()
        || value.as_bytes().contains(&b'/')
        || value.as_bytes().contains(&b':')
    {
        return Err(invalid_data(format!(
            "{label} is empty or contains a path/list separator"
        )));
    }
    Ok(())
}

fn set_once(slot: &mut Option<u64>, value: u64, message: &'static str) -> io::Result<()> {
    if slot.replace(value).is_some() {
        return Err(invalid_data(message));
    }
    Ok(())
}

fn validate_static_image(parsed: &ParsedElf) -> io::Result<()> {
    if !parsed.needed.is_empty() || parsed.has_injection_tags {
        return Err(invalid_data(
            "static sandbox monitor contains dynamic runtime dependencies",
        ));
    }
    Ok(())
}

fn validate_loader_image(parsed: &ParsedElf) -> io::Result<()> {
    if parsed.interpreter.is_some() {
        return Err(invalid_data(
            "sandbox monitor dynamic loader has a nested PT_INTERP",
        ));
    }
    if parsed.has_injection_tags {
        return Err(invalid_data(
            "sandbox monitor dynamic loader contains an injection tag",
        ));
    }
    Ok(())
}

fn vaddr_to_offset(vaddr: u64, size: u64, loads: &[(u64, u64, u64, u64)]) -> io::Result<u64> {
    for &(load_vaddr, mem_size, file_offset, file_size) in loads {
        let Some(relative) = vaddr.checked_sub(load_vaddr) else {
            continue;
        };
        if relative <= mem_size
            && relative
                .checked_add(size)
                .is_some_and(|end| end <= file_size)
        {
            return file_offset
                .checked_add(relative)
                .ok_or_else(|| invalid_data("ELF string-table file offset overflow"));
        }
    }
    Err(invalid_data(
        "ELF dynamic string table is outside a loadable segment",
    ))
}

fn pinned_current_inventory() -> io::Result<Vec<PinnedCandidate>> {
    pinned_inventory_from_maps(&read_maps_snapshot()?)
}

fn read_maps_snapshot() -> io::Result<Vec<u8>> {
    read_bounded(
        File::open("/proc/self/maps")?,
        MAX_MAPS_BYTES,
        "mapped runtime snapshot",
    )
}

fn read_bounded(mut input: impl Read, limit: usize, label: &str) -> io::Result<Vec<u8>> {
    let read_limit = limit
        .checked_add(1)
        .ok_or_else(|| invalid_data(format!("{label} byte budget overflow")))?;
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    input
        .by_ref()
        .take(read_limit as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid_data(format!("{label} exceeded its byte budget")));
    }
    Ok(bytes)
}

fn pinned_inventory_from_maps(bytes: &[u8]) -> io::Result<Vec<PinnedCandidate>> {
    let mut opened_paths = BTreeSet::<PathBuf>::new();
    let mut by_identity = BTreeMap::<(u64, u64), PinnedCandidate>::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let Some(record) = parse_maps_record(line)? else {
            continue;
        };
        if record.inode == 0
            || !record.path.starts_with(b"/")
            || record.path.ends_with(b" (deleted)")
        {
            continue;
        }
        let path = PathBuf::from(OsString::from_vec(decode_maps_path(record.path)));
        if !opened_paths.insert(path.clone()) {
            continue;
        }
        if opened_paths.len() > MAX_MAPPED_CANDIDATES {
            return Err(invalid_data("mapped runtime candidate budget exceeded"));
        }
        let file = File::open(&path)?;
        let file = duplicate_above_stdio(&file)?;
        let identity = FileIdentity::from_file(&file)?;
        let actual_device = (
            libc::major(identity.dev as libc::dev_t) as u64,
            libc::minor(identity.dev as libc::dev_t) as u64,
        );
        if identity.ino != record.inode || actual_device != record.device {
            return Err(invalid_data(
                "mapped runtime device/inode changed while pinning",
            ));
        }
        let alias = path
            .file_name()
            .ok_or_else(|| invalid_data("mapped runtime object has no basename"))?
            .to_owned();
        match by_identity.entry((identity.dev, identity.ino)) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(PinnedCandidate {
                    file,
                    source_path: path,
                    identity,
                    aliases: BTreeSet::from([alias]),
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().aliases.insert(alias);
            }
        }
        if by_identity.len() > MAX_RUNTIME_OBJECTS {
            return Err(invalid_data("mapped runtime object budget exceeded"));
        }
    }
    Ok(by_identity.into_values().collect())
}

fn parse_pinned_inventory(candidates: &[PinnedCandidate]) -> io::Result<Vec<InventoryObject>> {
    let mut inventory = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let file = duplicate_above_stdio(&candidate.file)?;
        let parsed = match parse_elf(&file) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        inventory.push(InventoryObject {
            file,
            path: candidate.source_path.clone(),
            aliases: candidate.aliases.clone(),
            identity: candidate.identity,
            parsed,
        });
    }
    Ok(inventory)
}

struct MapsRecord<'a> {
    device: (u64, u64),
    inode: u64,
    path: &'a [u8],
}

fn parse_maps_record(line: &[u8]) -> io::Result<Option<MapsRecord<'_>>> {
    let mut offset = 0;
    let mut next_field = || {
        while line.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        let start = offset;
        while line
            .get(offset)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            offset += 1;
        }
        (start != offset).then(|| &line[start..offset])
    };
    let Some(_range) = next_field() else {
        return Ok(None);
    };
    let Some(_permissions) = next_field() else {
        return Ok(None);
    };
    let Some(_file_offset) = next_field() else {
        return Ok(None);
    };
    let Some(device) = next_field() else {
        return Ok(None);
    };
    let Some(inode) = next_field() else {
        return Ok(None);
    };
    while line.get(offset).is_some_and(u8::is_ascii_whitespace) {
        offset += 1;
    }
    let path = &line[offset..];
    let (major, minor) = split_once_byte(device, b':')
        .ok_or_else(|| invalid_data("invalid device in /proc/self/maps"))?;
    let device = (parse_ascii_u64(major, 16)?, parse_ascii_u64(minor, 16)?);
    let inode = parse_ascii_u64(inode, 10)?;
    Ok(Some(MapsRecord {
        device,
        inode,
        path,
    }))
}

fn split_once_byte(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|byte| *byte == delimiter)?;
    Some((&bytes[..index], &bytes[index + 1..]))
}

fn parse_ascii_u64(bytes: &[u8], radix: u32) -> io::Result<u64> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_data("non-ASCII integer in /proc/self/maps"))?;
    u64::from_str_radix(text, radix).map_err(|_| invalid_data("invalid integer in /proc/self/maps"))
}

fn decode_maps_path(path: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut offset = 0;
    while offset < path.len() {
        if path[offset..].starts_with(b"\\012") {
            decoded.push(b'\n');
            offset += 4;
        } else {
            decoded.push(path[offset]);
            offset += 1;
        }
    }
    decoded
}

fn require_pinned_identity(
    inventory: &[PinnedCandidate],
    expected: FileIdentity,
    label: &str,
) -> io::Result<()> {
    if inventory
        .iter()
        .any(|object| object.identity.dev == expected.dev && object.identity.ino == expected.ino)
    {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "sandbox {label} identity is absent from the mapped runtime inventory"
        )))
    }
}

fn resolve_needed_closure(
    roots: &[&ParsedElf],
    inventory: &[InventoryObject],
) -> io::Result<(Vec<PinnedObject>, OsString)> {
    for root in roots {
        if root.has_injection_tags {
            return Err(invalid_data(
                "sandbox monitor runtime root contains a dynamic-loader injection tag",
            ));
        }
    }
    let mut names = BTreeMap::<OsString, Vec<&InventoryObject>>::new();
    for object in inventory {
        if object.parsed.has_injection_tags {
            continue;
        }
        for name in &object.aliases {
            names.entry(name.clone()).or_default().push(object);
        }
        if let Some(name) = &object.parsed.soname {
            names.entry(name.clone()).or_default().push(object);
        }
    }
    let mut queue = roots
        .iter()
        .flat_map(|root| root.needed.iter().cloned())
        .collect::<VecDeque<_>>();
    let mut selected = BTreeMap::<(u64, u64), (&InventoryObject, BTreeSet<OsString>)>::new();
    while let Some(name) = queue.pop_front() {
        let Some(candidates) = names.get(&name) else {
            return Err(invalid_data("ELF runtime dependency is not mapped"));
        };
        let identities = candidates
            .iter()
            .map(|candidate| (candidate.identity.dev, candidate.identity.ino))
            .collect::<BTreeSet<_>>();
        if identities.len() != 1 {
            return Err(invalid_data("ELF runtime dependency is ambiguous"));
        }
        let candidate = candidates[0];
        let key = (candidate.identity.dev, candidate.identity.ino);
        if let Some((_, aliases)) = selected.get_mut(&key) {
            aliases.insert(name);
        } else {
            if candidate.parsed.has_injection_tags {
                return Err(invalid_data(
                    "ELF runtime dependency contains a loader injection tag",
                ));
            }
            selected.insert(key, (candidate, BTreeSet::from([name])));
            queue.extend(candidate.parsed.needed.iter().cloned());
        }
        if selected.len() > MAX_RUNTIME_OBJECTS {
            return Err(invalid_data("ELF runtime closure budget exceeded"));
        }
    }
    let mut objects = Vec::with_capacity(selected.len());
    let mut inhibit = BTreeSet::<OsString>::new();
    let private_root = Path::new(PRIVATE_RUNTIME_ROOT);
    inhibit.insert(private_root.join("nub-monitor").into_os_string());
    inhibit.insert(OsString::from("nub-monitor"));
    for (object, aliases) in selected.into_values() {
        for private_name in aliases {
            inhibit.insert(private_name.clone());
            inhibit.insert(
                private_root
                    .join("lib")
                    .join(&private_name)
                    .into_os_string(),
            );
            objects.push(PinnedObject {
                file: duplicate_above_stdio(&object.file)?,
                source_path: object.path.clone(),
                identity: object.identity,
                private_name,
            });
            if objects.len() > MAX_RUNTIME_OBJECTS {
                return Err(invalid_data("ELF runtime alias budget exceeded"));
            }
        }
    }
    objects.sort_by(|left, right| left.private_name.cmp(&right.private_name));
    let mut inhibit_bytes = Vec::new();
    for (index, name) in inhibit.into_iter().enumerate() {
        if index != 0 {
            inhibit_bytes.push(b':');
        }
        inhibit_bytes.extend_from_slice(name.as_bytes());
    }
    Ok((objects, OsString::from_vec(inhibit_bytes)))
}

fn loader_family(path: &Path) -> io::Result<LoaderFamily> {
    let bytes = path.as_os_str().as_bytes();
    if bytes
        .windows(b"ld-musl".len())
        .any(|part| part == b"ld-musl")
    {
        return Ok(LoaderFamily::Musl);
    }
    if bytes
        .windows(b"ld-linux".len())
        .any(|part| part == b"ld-linux")
        || bytes.ends_with(b"/ld.so")
    {
        return Ok(LoaderFamily::Glibc);
    }
    Err(invalid_data("unsupported sandbox monitor dynamic loader"))
}

fn require_proven_loader_search(family: LoaderFamily) -> io::Result<()> {
    match family {
        LoaderFamily::Glibc => Ok(()),
        LoaderFamily::Musl => Err(invalid_data(
            "monitor-runtime-musl-search: musl loader search isolation is not proven",
        )),
    }
}

fn pin_path(path: &Path, private_name: &OsStr) -> io::Result<PinnedObject> {
    let file = File::open(path)?;
    let file = duplicate_above_stdio(&file)?;
    let identity = FileIdentity::from_file(&file)?;
    let source_path = fs::read_link(path)
        .ok()
        .filter(|resolved| resolved.is_absolute())
        .or_else(|| fs::canonicalize(path).ok())
        .unwrap_or_else(|| path.to_path_buf());
    Ok(PinnedObject {
        file,
        source_path,
        identity,
        private_name: private_name.to_owned(),
    })
}

fn duplicate_pinned(object: &PinnedObject) -> io::Result<PinnedObject> {
    Ok(PinnedObject {
        file: duplicate_above_stdio(&object.file)?,
        source_path: object.source_path.clone(),
        identity: object.identity,
        private_name: object.private_name.clone(),
    })
}

pub(super) fn duplicate_above_stdio(file: &File) -> io::Result<File> {
    duplicate_file_at_least(file, 3)
}

fn duplicate_file_at_least(file: &File, minimum: RawFd) -> io::Result<File> {
    duplicate_fd_at_least(file.as_raw_fd(), minimum).map(File::from)
}

fn relocate_file_at_least(file: File, minimum: RawFd) -> io::Result<File> {
    let relocated = duplicate_file_at_least(&file, minimum)?;
    drop(file);
    Ok(relocated)
}

fn duplicate_fd_at_least(fd: RawFd, minimum: RawFd) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn relocate_fd_at_least(fd: OwnedFd, minimum: RawFd) -> io::Result<OwnedFd> {
    let relocated = duplicate_fd_at_least(fd.as_raw_fd(), minimum)?;
    drop(fd);
    Ok(relocated)
}

fn runtime_build_marker(objects: &[RuntimeObject]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"nub-sandbox-monitor-runtime-manifest-v1");
    hasher.update(PROTOCOL_VERSION.to_le_bytes());
    let mut objects = objects.iter().collect::<Vec<_>>();
    objects.sort_by(|left, right| left.path.cmp(&right.path));
    hasher.update((objects.len() as u64).to_le_bytes());
    for object in objects {
        let path = object.path.as_os_str().as_bytes();
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        hasher.update(object.identity.dev.to_le_bytes());
        hasher.update(object.identity.ino.to_le_bytes());
        hasher.update(object.identity.size.to_le_bytes());
    }
    hasher.finalize().into()
}

fn validate_runtime_build_marker(
    objects: &[RuntimeObject],
    expected: &[u8; 32],
) -> io::Result<[u8; 32]> {
    let marker = runtime_build_marker(objects);
    if !constant_time_eq(&marker, expected) {
        return Err(invalid_data(
            "sandbox monitor runtime build marker mismatch",
        ));
    }
    Ok(marker)
}

fn describe_current_runtime() -> io::Result<()> {
    let inventory = pinned_current_inventory()?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(DESCRIBE_MAGIC)?;
    stdout.write_all(&PROTOCOL_VERSION.to_le_bytes())?;
    let count = u32::try_from(inventory.len())
        .map_err(|_| invalid_data("runtime description object count overflow"))?;
    stdout.write_all(&count.to_le_bytes())?;
    for object in inventory {
        let path = object.source_path.as_os_str().as_bytes();
        let len = u32::try_from(path.len())
            .map_err(|_| invalid_data("runtime description path too long"))?;
        stdout.write_all(&len.to_le_bytes())?;
        stdout.write_all(path)?;
        stdout.write_all(&object.identity.dev.to_le_bytes())?;
        stdout.write_all(&object.identity.ino.to_le_bytes())?;
        stdout.write_all(&object.identity.size.to_le_bytes())?;
        let alias_count = u32::try_from(object.aliases.len())
            .map_err(|_| invalid_data("runtime description alias count overflow"))?;
        stdout.write_all(&alias_count.to_le_bytes())?;
        for alias in object.aliases {
            let bytes = alias.as_bytes();
            let len = u32::try_from(bytes.len())
                .map_err(|_| invalid_data("runtime description alias too long"))?;
            stdout.write_all(&len.to_le_bytes())?;
            stdout.write_all(bytes)?;
        }
    }
    stdout.flush()
}

fn describe_runtime(executable: &PinnedObject) -> io::Result<Vec<PinnedCandidate>> {
    use std::process::{Command, Stdio};
    let path = PathBuf::from(format!("/proc/self/fd/{}", executable.file.as_raw_fd()));
    let mut command = Command::new(path);
    command
        .env_clear()
        .arg(DESCRIBE_SENTINEL)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        use std::os::unix::process::CommandExt;
        let fd = executable.file.as_raw_fd();
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let output = capture_child_bounded(child)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "monitor runtime description failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut cursor = Cursor::new(&output.stdout);
    cursor.expect(DESCRIBE_MAGIC)?;
    if cursor.u16()? != PROTOCOL_VERSION {
        return Err(invalid_data("monitor runtime description version mismatch"));
    }
    let count = cursor.count()?;
    if count > MAX_RUNTIME_OBJECTS {
        return Err(invalid_data(
            "monitor runtime description object budget exceeded",
        ));
    }
    let mut inventory = Vec::with_capacity(count);
    for _ in 0..count {
        let path = PathBuf::from(OsString::from_vec(cursor.bytes()?.to_vec()));
        if !path.is_absolute() {
            return Err(invalid_data("monitor runtime description path is relative"));
        }
        let expected = cursor.identity()?;
        let file = File::open(&path)?;
        let file = duplicate_above_stdio(&file)?;
        let identity = FileIdentity::from_file(&file)?;
        if identity != expected {
            return Err(invalid_data("monitor runtime description identity changed"));
        }
        let alias_count = cursor.count()?;
        if alias_count == 0 || alias_count > MAX_MAPPED_CANDIDATES {
            return Err(invalid_data(
                "monitor runtime description alias budget is invalid",
            ));
        }
        let mut aliases = BTreeSet::new();
        for _ in 0..alias_count {
            let alias = OsString::from_vec(cursor.bytes()?.to_vec());
            validate_loader_object_name(&alias, "runtime description alias")?;
            if !aliases.insert(alias) {
                return Err(invalid_data("duplicate monitor runtime description alias"));
            }
        }
        inventory.push(PinnedCandidate {
            file,
            source_path: path,
            identity,
            aliases,
        });
    }
    cursor.finish()?;
    Ok(inventory)
}

struct BoundedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn capture_child_bounded(mut child: std::process::Child) -> io::Result<BoundedOutput> {
    let Some(mut stdout) = child.stdout.take() else {
        terminate_description_group(&mut child);
        return Err(invalid_data(
            "monitor runtime description stdout is not piped",
        ));
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_description_group(&mut child);
        return Err(invalid_data(
            "monitor runtime description stderr is not piped",
        ));
    };
    if let Err(error) =
        set_nonblocking_fd(stdout.as_raw_fd()).and_then(|_| set_nonblocking_fd(stderr.as_raw_fd()))
    {
        terminate_description_group(&mut child);
        return Err(error);
    }
    let deadline = Instant::now() + DESCRIBE_TIMEOUT;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;
    let result = (|| {
        loop {
            if !stdout_eof {
                stdout_eof =
                    drain_description_pipe(&mut stdout, &mut stdout_bytes, MAX_BOOTSTRAP_BYTES)?;
            }
            if !stderr_eof {
                stderr_eof =
                    drain_description_pipe(&mut stderr, &mut stderr_bytes, MAX_FRAME_PAYLOAD)?;
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if stdout_eof
                && stderr_eof
                && let Some(status) = status
            {
                break Ok(status);
            }
            let now = Instant::now();
            if now >= deadline {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "monitor runtime description exceeded its deadline",
                ));
            }
            poll_description_pipes(
                stdout.as_raw_fd(),
                stderr.as_raw_fd(),
                (deadline - now).min(Duration::from_millis(10)),
            )?;
        }
    })();
    match result {
        Ok(status) => Ok(BoundedOutput {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        }),
        Err(error) => {
            terminate_description_group(&mut child);
            drain_description_after_kill(&mut stdout, &mut stderr);
            Err(error)
        }
    }
}

fn set_nonblocking_fd(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_description_pipe(
    pipe: &mut impl Read,
    output: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(count) => extend_description_output(output, &chunk[..count], limit)?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn extend_description_output(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> io::Result<()> {
    if output
        .len()
        .checked_add(bytes.len())
        .is_none_or(|length| length > limit)
    {
        return Err(invalid_data(
            "monitor runtime description exceeded its output budget",
        ));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn poll_description_pipes(stdout: RawFd, stderr: RawFd, timeout: Duration) -> io::Result<()> {
    let mut fds = [
        libc::pollfd {
            fd: stdout,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
    ];
    let timeout = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } >= 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
        Ok(())
    } else {
        Err(error)
    }
}

fn terminate_description_group(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

fn drain_description_after_kill(stdout: &mut impl Read, stderr: &mut impl Read) {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut bytes = [0u8; 8192];
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    while Instant::now() < deadline && !(stdout_eof && stderr_eof) {
        if !stdout_eof {
            stdout_eof = drain_to_eof_or_would_block(stdout, &mut bytes);
        }
        if !stderr_eof {
            stderr_eof = drain_to_eof_or_would_block(stderr, &mut bytes);
        }
        if !(stdout_eof && stderr_eof) {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn drain_to_eof_or_would_block(pipe: &mut impl Read, bytes: &mut [u8]) -> bool {
    loop {
        match pipe.read(bytes) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

fn validate_bootstrap_strings(
    program: &OsStr,
    args: &[OsString],
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
) -> io::Result<()> {
    if program.as_bytes().is_empty() || program.as_bytes().contains(&0) {
        return Err(invalid_data("invalid sandbox target program"));
    }
    if !Path::new(program).is_absolute() {
        return Err(invalid_data("sandbox target program is not absolute"));
    }
    if cwd.as_os_str().as_bytes().is_empty() || cwd.as_os_str().as_bytes().contains(&0) {
        return Err(invalid_data("invalid sandbox target cwd"));
    }
    if !cwd.is_absolute() {
        return Err(invalid_data("sandbox target cwd is not absolute"));
    }
    for arg in args {
        if arg.as_bytes().contains(&0) {
            return Err(invalid_data("sandbox target argument contains NUL"));
        }
    }
    for (key, value) in env {
        if key.as_bytes().is_empty()
            || key.as_bytes().contains(&0)
            || key.as_bytes().contains(&b'=')
            || value.as_bytes().contains(&0)
        {
            return Err(invalid_data("invalid sandbox target environment"));
        }
    }
    Ok(())
}

fn monitor_main() -> io::Result<i32> {
    arm_monitor_parent_death()?;
    harden_monitor_before_secrets()?;
    let spec = read_sealed_bootstrap(BOOTSTRAP_FD)?;
    validate_pipe_endpoint(RELEASE_FD, false, false, "release gate")?;
    validate_pipe_endpoint(SIGNAL_RELAY_FD, true, true, "signal relay")?;
    let verified_build_marker = verify_monitor_runtime_identity(&spec)?;
    let expected_uid = unsafe { libc::getuid() };
    validate_raw_control_fd(CONTROL_FD, expected_uid)?;
    set_control_timeouts(CONTROL_FD, DESCRIBE_TIMEOUT)?;
    let control_fd = unsafe { OwnedFd::from_raw_fd(CONTROL_FD) };
    let mut control = ControlChannel::new(
        control_fd,
        spec.session,
        ExpectedPeer {
            // An ancestor-namespace sender may not have a PID mapping in the
            // monitor's namespace. Descriptor possession + session/release
            // capabilities authenticate that direction; UID still must map.
            pid: None,
            uid: expected_uid,
        },
    )?;
    let close_result = (|| {
        if unsafe { libc::close(BOOTSTRAP_FD) } != 0 {
            return Err(io::Error::last_os_error());
        }
        close_all_except(&[
            libc::STDIN_FILENO,
            libc::STDOUT_FILENO,
            libc::STDERR_FILENO,
            CONTROL_FD,
            RELEASE_FD,
            SIGNAL_RELAY_FD,
        ])
    })();
    if let Err(error) = close_result {
        send_fatal(&mut control, &error);
        return Err(error);
    }

    let mut state = MonitorState::Bootstrap;
    let outcome = (|| {
        run_release_states(&spec, verified_build_marker, &mut control, &mut state)?;
        run_target_stopped_state(&spec, &mut control, &mut state)
    })();
    if let Err(error) = &outcome {
        send_fatal(&mut control, error);
    }
    outcome.map(|_| 0)
}

fn arm_monitor_parent_death() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut signal = 0 as libc::c_int;
    if unsafe {
        libc::prctl(
            libc::PR_GET_PDEATHSIG,
            (&mut signal as *mut libc::c_int) as libc::c_ulong,
            0,
            0,
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if signal != libc::SIGKILL {
        return Err(invalid_data(
            "sandbox monitor parent-death signal was not armed",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorState {
    Bootstrap,
    MonitorReadyAwaitingRelease,
    MonitorReleased,
    TargetStopped,
    TargetStarting,
    TargetRunning,
    TargetExitedAwaitingCompletion,
    SessionCompletionAuthorized,
    CleanupCompletePublished,
    TargetStartFailed,
}

fn transition_monitor_state(
    state: &mut MonitorState,
    expected: MonitorState,
    next: MonitorState,
) -> io::Result<()> {
    if *state != expected {
        return Err(invalid_data("invalid sandbox monitor state transition"));
    }
    *state = next;
    Ok(())
}

fn run_release_states(
    spec: &BootstrapSpec,
    verified_build_marker: [u8; 32],
    control: &mut ControlChannel,
    state: &mut MonitorState,
) -> io::Result<()> {
    let mut ready = Vec::with_capacity(56);
    put_identity(&mut ready, spec.executable);
    ready.extend_from_slice(&verified_build_marker);
    control.send(FrameKind::MonitorReady, ready)?;
    transition_monitor_state(
        state,
        MonitorState::Bootstrap,
        MonitorState::MonitorReadyAwaitingRelease,
    )?;
    read_exact_capability_and_eof(RELEASE_FD, &spec.release)?;
    if unsafe { libc::close(RELEASE_FD) } != 0 {
        return Err(io::Error::last_os_error());
    }
    transition_monitor_state(
        state,
        MonitorState::MonitorReadyAwaitingRelease,
        MonitorState::MonitorReleased,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetStoppedAttestation {
    namespace_pid: u32,
    starttime: u64,
    start_challenge: [u8; START_CHALLENGE_LEN],
}

impl TargetStoppedAttestation {
    const ENCODED_LEN: usize = 12 + START_CHALLENGE_LEN;

    fn encode(self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(Self::ENCODED_LEN);
        payload.extend_from_slice(&self.namespace_pid.to_le_bytes());
        payload.extend_from_slice(&self.starttime.to_le_bytes());
        payload.extend_from_slice(&self.start_challenge);
        payload
    }

    fn decode(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != Self::ENCODED_LEN {
            return Err(invalid_data("invalid stopped-target attestation length"));
        }
        let namespace_pid = u32::from_le_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| invalid_data("invalid stopped-target PID"))?,
        );
        let starttime = u64::from_le_bytes(
            payload[4..12]
                .try_into()
                .map_err(|_| invalid_data("invalid stopped-target starttime"))?,
        );
        if namespace_pid <= 1 || starttime == 0 {
            return Err(invalid_data("invalid stopped-target identity"));
        }
        Ok(Self {
            namespace_pid,
            starttime,
            start_challenge: payload[12..]
                .try_into()
                .map_err(|_| invalid_data("invalid target start challenge"))?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetExecFailure {
    stage: TargetSetupStage,
    errno: libc::c_int,
    raw_status: libc::c_int,
}

impl TargetExecFailure {
    fn encode(self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(TARGET_EXEC_FAILURE_LEN);
        payload.extend_from_slice(&(self.stage as u16).to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&self.errno.to_le_bytes());
        payload.extend_from_slice(&self.raw_status.to_le_bytes());
        payload
    }

    fn decode(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != TARGET_EXEC_FAILURE_LEN {
            return Err(invalid_data("invalid target exec-failure length"));
        }
        let stage = TargetSetupStage::from_raw(u16::from_le_bytes([payload[0], payload[1]]))
            .ok_or_else(|| invalid_data("invalid target exec-failure stage"))?;
        let reserved = u16::from_le_bytes([payload[2], payload[3]]);
        let errno = libc::c_int::from_le_bytes(
            payload[4..8]
                .try_into()
                .map_err(|_| invalid_data("invalid target exec-failure errno"))?,
        );
        let raw_status = libc::c_int::from_le_bytes(
            payload[8..12]
                .try_into()
                .map_err(|_| invalid_data("invalid target exec-failure wait status"))?,
        );
        if reserved != 0
            || stage != TargetSetupStage::Execve
            || errno <= 0
            || !(libc::WIFEXITED(raw_status) || libc::WIFSIGNALED(raw_status))
        {
            return Err(invalid_data("malformed target exec-failure payload"));
        }
        Ok(Self {
            stage,
            errno,
            raw_status,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetExitedReport {
    raw_status: libc::c_int,
    descendants_reaped: u32,
}

impl TargetExitedReport {
    fn encode(self) -> io::Result<Vec<u8>> {
        validate_terminal_wait_status(self.raw_status)?;
        let mut payload = Vec::with_capacity(TARGET_EXITED_LEN);
        payload.extend_from_slice(&self.raw_status.to_le_bytes());
        payload.extend_from_slice(&self.descendants_reaped.to_le_bytes());
        Ok(payload)
    }

    fn decode(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != TARGET_EXITED_LEN {
            return Err(invalid_data("invalid target-exited report length"));
        }
        let raw_status = libc::c_int::from_le_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| invalid_data("target-exited status is truncated"))?,
        );
        validate_terminal_wait_status(raw_status)?;
        let descendants_reaped = u32::from_le_bytes(
            payload[4..]
                .try_into()
                .map_err(|_| invalid_data("target-exited reap count is truncated"))?,
        );
        Ok(Self {
            raw_status,
            descendants_reaped,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompletionAttestation {
    report: TargetExitedReport,
    challenge: [u8; COMPLETION_CHALLENGE_LEN],
}

impl CompletionAttestation {
    fn encode(self) -> io::Result<Vec<u8>> {
        let mut payload = self.report.encode()?;
        payload.extend_from_slice(&self.challenge);
        debug_assert_eq!(payload.len(), COMPLETION_ATTESTATION_LEN);
        Ok(payload)
    }

    fn decode(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != COMPLETION_ATTESTATION_LEN {
            return Err(invalid_data(
                "invalid sandbox completion attestation length",
            ));
        }
        let report = TargetExitedReport::decode(&payload[..TARGET_EXITED_LEN])?;
        let challenge = payload[TARGET_EXITED_LEN..]
            .try_into()
            .map_err(|_| invalid_data("sandbox completion challenge is truncated"))?;
        Ok(Self { report, challenge })
    }

    fn require_exact_echo(self, payload: &[u8]) -> io::Result<()> {
        let echoed = Self::decode(payload)?;
        if echoed.report != self.report || !constant_time_eq(&echoed.challenge, &self.challenge) {
            return Err(invalid_data(
                "sandbox completion request did not echo the exact attestation",
            ));
        }
        Ok(())
    }
}

fn validate_terminal_wait_status(status: libc::c_int) -> io::Result<()> {
    if status < 0 {
        return Err(invalid_data("target-exited status is negative"));
    }
    let raw = status as u32;
    let signal = raw & 0x7f;
    let valid = if signal == 0 {
        // A normal exit uses only the eight-bit exit code in bits 8..=15.
        raw & 0xff == 0 && raw & !0xffff == 0
    } else {
        // A signaled exit uses only a terminating Linux signal plus the core
        // bit, and that bit is possible only for the core-dumping defaults.
        let core_dumped = raw & 0x80 != 0;
        signal_can_terminate(signal)
            && (!core_dumped || signal_can_dump_core(signal))
            && raw & !0xff == 0
    };
    if !valid || raw == 0xffff {
        return Err(invalid_data(
            "target-exited status is not an exact terminal wait status",
        ));
    }
    Ok(())
}

fn signal_can_terminate(signal: u32) -> bool {
    signal > 0
        && signal < LINUX_NSIG as u32
        && !matches!(
            signal as libc::c_int,
            libc::SIGCHLD
                | libc::SIGCONT
                | libc::SIGSTOP
                | libc::SIGTSTP
                | libc::SIGTTIN
                | libc::SIGTTOU
                | libc::SIGURG
                | libc::SIGWINCH
        )
}

fn signal_can_dump_core(signal: u32) -> bool {
    matches!(
        signal as libc::c_int,
        libc::SIGABRT
            | libc::SIGBUS
            | libc::SIGFPE
            | libc::SIGILL
            | libc::SIGQUIT
            | libc::SIGSEGV
            | libc::SIGSYS
            | libc::SIGTRAP
            | libc::SIGXCPU
            | libc::SIGXFSZ
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SignalRelayRecord {
    sequence: u64,
    signal: libc::c_int,
}

impl SignalRelayRecord {
    fn encode(self) -> io::Result<[u8; SIGNAL_RELAY_RECORD_LEN]> {
        validate_forwarded_signal(self.signal)?;
        let mut record = [0u8; SIGNAL_RELAY_RECORD_LEN];
        record[..4].copy_from_slice(SIGNAL_RELAY_MAGIC);
        record[4..12].copy_from_slice(&self.sequence.to_le_bytes());
        record[12..].copy_from_slice(&self.signal.to_le_bytes());
        Ok(record)
    }

    fn decode(record: &[u8], expected_sequence: u64) -> io::Result<Self> {
        if record.len() != SIGNAL_RELAY_RECORD_LEN || &record[..4] != SIGNAL_RELAY_MAGIC {
            return Err(invalid_data("sandbox signal relay record is malformed"));
        }
        let sequence = u64::from_le_bytes(
            record[4..12]
                .try_into()
                .map_err(|_| invalid_data("sandbox signal relay sequence is truncated"))?,
        );
        if sequence != expected_sequence {
            return Err(invalid_data(
                "sandbox signal relay sequence mismatch or replay",
            ));
        }
        let signal = libc::c_int::from_le_bytes(
            record[12..]
                .try_into()
                .map_err(|_| invalid_data("sandbox signal relay value is truncated"))?,
        );
        validate_forwarded_signal(signal)?;
        Ok(Self { sequence, signal })
    }
}

fn validate_forwarded_signal(signal: libc::c_int) -> io::Result<()> {
    if matches!(
        signal,
        libc::SIGHUP
            | libc::SIGINT
            | libc::SIGQUIT
            | libc::SIGTERM
            | libc::SIGUSR1
            | libc::SIGUSR2
            | libc::SIGCONT
    ) {
        Ok(())
    } else {
        Err(invalid_data("sandbox signal relay value is unsupported"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum TargetSetupStage {
    ProcessGroup = 1,
    WorkingDirectory = 2,
    NoNewPrivileges = 3,
    Seccomp = 4,
    DescriptorSweep = 5,
    Stop = 6,
    StartGate = 7,
    Dumpable = 8,
    Execve = 9,
    ParentDeath = 10,
}

impl TargetSetupStage {
    fn label(self) -> &'static str {
        match self {
            Self::ProcessGroup => "process-group",
            Self::WorkingDirectory => "working-directory",
            Self::NoNewPrivileges => "no-new-privileges",
            Self::Seccomp => "seccomp",
            Self::DescriptorSweep => "descriptor-sweep",
            Self::Stop => "initial-stop",
            Self::StartGate => "private-start-gate",
            Self::Dumpable => "target-dumpability",
            Self::Execve => "execve",
            Self::ParentDeath => "parent-death",
        }
    }

    fn from_raw(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::ProcessGroup),
            2 => Some(Self::WorkingDirectory),
            3 => Some(Self::NoNewPrivileges),
            4 => Some(Self::Seccomp),
            5 => Some(Self::DescriptorSweep),
            6 => Some(Self::Stop),
            7 => Some(Self::StartGate),
            8 => Some(Self::Dumpable),
            9 => Some(Self::Execve),
            10 => Some(Self::ParentDeath),
            _ => None,
        }
    }
}

struct StoppedTarget {
    pid: libc::pid_t,
    gate_writer: Option<File>,
    error_reader: File,
    target_status: Option<libc::c_int>,
    descendants_reaped: u32,
    all_reaped: bool,
}

struct PreparedTargetExec {
    cwd: CString,
    program: CString,
    _argv_storage: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    _env_storage: Vec<CString>,
    envp: Vec<*const libc::c_char>,
}

#[derive(Clone, Copy)]
struct TargetExecPointers {
    cwd: *const libc::c_char,
    program: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
}

impl PreparedTargetExec {
    fn new(spec: &BootstrapSpec) -> io::Result<Self> {
        let cwd = CString::new(spec.cwd.as_os_str().as_bytes())
            .map_err(|_| invalid_data("sandbox target cwd contains NUL"))?;
        let program = CString::new(spec.program.as_bytes())
            .map_err(|_| invalid_data("sandbox target program contains NUL"))?;

        let mut argv_storage = Vec::with_capacity(spec.args.len() + 1);
        argv_storage.push(
            CString::new(spec.program.as_bytes())
                .map_err(|_| invalid_data("sandbox target program contains NUL"))?,
        );
        for argument in &spec.args {
            argv_storage.push(
                CString::new(argument.as_bytes())
                    .map_err(|_| invalid_data("sandbox target argument contains NUL"))?,
            );
        }
        let mut argv = argv_storage
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();
        argv.push(std::ptr::null());

        let mut env_storage = Vec::with_capacity(spec.env.len());
        for (key, value) in &spec.env {
            let mut entry = Vec::with_capacity(key.as_bytes().len() + value.as_bytes().len() + 1);
            entry.extend_from_slice(key.as_bytes());
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            env_storage.push(
                CString::new(entry)
                    .map_err(|_| invalid_data("sandbox target environment contains NUL"))?,
            );
        }
        let mut envp = env_storage
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();
        envp.push(std::ptr::null());

        Ok(Self {
            cwd,
            program,
            _argv_storage: argv_storage,
            argv,
            _env_storage: env_storage,
            envp,
        })
    }

    fn pointers(&self) -> TargetExecPointers {
        TargetExecPointers {
            cwd: self.cwd.as_ptr(),
            program: self.program.as_ptr(),
            argv: self.argv.as_ptr(),
            envp: self.envp.as_ptr(),
        }
    }
}

impl StoppedTarget {
    fn wait_for_initial_stop(
        &mut self,
        uses_seccomp: bool,
        gate_reader_fd: RawFd,
        error_writer_fd: RawFd,
    ) -> io::Result<TargetStoppedAttestation> {
        let mut status = 0;
        let deadline = Instant::now() + DESCRIBE_TIMEOUT;
        let waited = loop {
            let waited =
                unsafe { libc::waitpid(self.pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if waited == self.pid {
                break waited;
            }
            if waited < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the target's initial stop",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        };
        ensure_before_deadline(deadline, "timed out waiting for the target's initial stop")?;
        if waited != self.pid {
            return Err(invalid_data("monitor waited for an unexpected target"));
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            self.target_status = Some(status);
            return Err(read_target_setup_error(
                self.error_reader.as_raw_fd(),
                status,
            ));
        }
        if !libc::WIFSTOPPED(status) || libc::WSTOPSIG(status) != libc::SIGSTOP {
            return Err(invalid_data("target did not enter its initial SIGSTOP"));
        }
        verify_stopped_target(self.pid, uses_seccomp, gate_reader_fd, error_writer_fd)
    }

    fn resume_and_release(&mut self) -> io::Result<()> {
        let mut gate_writer = self
            .gate_writer
            .take()
            .ok_or_else(|| invalid_data("target start gate was already released"))?;
        if unsafe { libc::kill(self.pid, libc::SIGCONT) } != 0 {
            return Err(io::Error::last_os_error());
        }
        gate_writer.write_all(&[START_GATE_BYTE])?;
        drop(gate_writer);
        Ok(())
    }

    fn poll_target_wait(&mut self) -> io::Result<Option<libc::c_int>> {
        if let Some(status) = self.target_status {
            return Ok(Some(status));
        }
        let mut status = 0;
        loop {
            let waited =
                unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
            if waited == 0 {
                return Ok(None);
            }
            if waited == self.pid {
                if libc::WIFSTOPPED(status) {
                    return Err(invalid_data(
                        "target stopped again during the exec transition",
                    ));
                }
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    self.target_status = Some(status);
                    return Ok(Some(status));
                }
                return Err(invalid_data(
                    "target produced an unexpected wait result during exec",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                return self
                    .target_status
                    .map(Some)
                    .ok_or_else(|| invalid_data("target wait authority disappeared"));
            }
            return Err(error);
        }
    }

    fn drain_running_wait_events(&mut self) -> io::Result<bool> {
        for _ in 0..MAX_RUNNING_EVENTS_PER_TURN {
            let mut status = 0;
            let waited = unsafe {
                libc::waitpid(
                    -1,
                    &mut status,
                    libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                )
            };
            if waited == 0 {
                return Ok(self.target_status.is_some());
            }
            if waited > 0 {
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    if waited == self.pid {
                        self.target_status = Some(status);
                    } else {
                        self.descendants_reaped =
                            self.descendants_reaped.checked_add(1).ok_or_else(|| {
                                invalid_data("sandbox descendant reap count overflow")
                            })?;
                    }
                } else if !(libc::WIFSTOPPED(status) || libc::WIFCONTINUED(status)) {
                    return Err(invalid_data(
                        "sandbox target tree produced an unexpected wait result",
                    ));
                }
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                if self.target_status.is_none() {
                    return Err(invalid_data(
                        "sandbox target wait authority disappeared while running",
                    ));
                }
                self.all_reaped = true;
                return Ok(true);
            }
            return Err(error);
        }
        Ok(self.target_status.is_some())
    }

    fn forward_running_signal(&mut self, signal: libc::c_int) -> io::Result<()> {
        if self.target_status.is_some() {
            return Ok(());
        }
        if unsafe { libc::kill(-self.pid, signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) && self.drain_running_wait_events()? {
            return Ok(());
        }
        Err(error)
    }

    fn await_exec_outcome(
        &mut self,
        control: &mut ControlChannel,
        deadline: Instant,
    ) -> io::Result<TargetExecOutcome> {
        let mut record = [0u8; TARGET_SETUP_ERROR_LEN + 1];
        let mut record_len = 0usize;
        let mut error_eof = false;
        loop {
            ensure_before_deadline(deadline, "timed out awaiting target exec acceptance")?;
            let terminal_status = self.poll_target_wait()?;
            loop {
                let read = unsafe {
                    libc::read(
                        self.error_reader.as_raw_fd(),
                        record[record_len..].as_mut_ptr().cast(),
                        record.len() - record_len,
                    )
                };
                if read > 0 {
                    record_len += read as usize;
                    if record_len == record.len() {
                        return Err(invalid_data(
                            "target exec-error record exceeded its fixed budget",
                        ));
                    }
                    continue;
                }
                if read == 0 {
                    error_eof = true;
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(error);
            }

            if error_eof {
                if record_len == 0 {
                    // The conventional exec-error pipe: successful exec closes the
                    // CLOEXEC writer. That zero-byte EOF remains authoritative when
                    // a fast target has already exited; poll_target_wait retained its
                    // status for the authenticated running-state transition.
                    ensure_before_deadline(deadline, "timed out awaiting target exec acceptance")?;
                    require_quiet_exec_control(control, deadline)?;
                    return Ok(TargetExecOutcome::Accepted);
                }
                if record_len != TARGET_SETUP_ERROR_LEN {
                    return Err(invalid_data("target exec-error record was truncated"));
                }
                let (stage, errno) = decode_target_setup_record(&record[..record_len])?;
                if stage != TargetSetupStage::Execve {
                    return Err(invalid_data(
                        "target reported a non-exec setup failure after its initial stop",
                    ));
                }
                if let Some(raw_status) = terminal_status {
                    ensure_before_deadline(deadline, "timed out awaiting target exec acceptance")?;
                    require_quiet_exec_control(control, deadline)?;
                    return Ok(TargetExecOutcome::Failed(TargetExecFailure {
                        stage,
                        errno,
                        raw_status,
                    }));
                }
            }

            ensure_before_deadline(deadline, "timed out awaiting target exec acceptance")?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = remaining.as_millis().clamp(1, 10) as libc::c_int;
            let mut pollfds = [
                libc::pollfd {
                    fd: self.error_reader.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
                libc::pollfd {
                    fd: control.fd.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
            ];
            let polled = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, timeout) };
            if polled < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if pollfds[1].revents != 0 {
                ensure_before_deadline(deadline, "timed out awaiting target exec acceptance")?;
                let frame = control.receive_with_deadline(Some(deadline))?;
                return Err(invalid_data(format!(
                    "unexpected {:?} control frame during target exec",
                    frame.kind
                )));
            }
        }
    }

    fn kill_and_reap_all(&mut self) -> io::Result<()> {
        self.kill_and_reap_all_until(Instant::now() + DESCRIBE_TIMEOUT)
    }

    fn kill_and_reap_all_until(&mut self, deadline: Instant) -> io::Result<()> {
        ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
        if self.all_reaped {
            return Ok(());
        }
        self.gate_writer.take();
        let mut first_error = None;
        for pid in [-self.pid, self.pid, -1] {
            ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
            let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
            let kill_error = (killed != 0).then(io::Error::last_os_error);
            ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
            if let Some(error) = kill_error
                && error.raw_os_error() != Some(libc::ESRCH)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        loop {
            ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
            let mut status = 0;
            let waited = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
            if waited > 0 {
                if waited == self.pid {
                    self.target_status = Some(status);
                } else if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    self.descendants_reaped = self
                        .descendants_reaped
                        .checked_add(1)
                        .ok_or_else(|| invalid_data("sandbox descendant reap count overflow"))?;
                }
                continue;
            }
            if waited < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.all_reaped = true;
                    ensure_before_deadline(
                        deadline,
                        "timed out reaping all sandbox target descendants",
                    )?;
                    return first_error.map_or(Ok(()), Err);
                }
                return Err(error);
            }
            ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
            let _ = unsafe { libc::kill(-1, libc::SIGKILL) };
            ensure_before_deadline(deadline, "timed out reaping all sandbox target descendants")?;
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for StoppedTarget {
    fn drop(&mut self) {
        let _ = self.kill_and_reap_all();
    }
}

enum TargetExecOutcome {
    Accepted,
    Failed(TargetExecFailure),
}

struct RunningBoundaryInputs {
    signal_sequence: u64,
    terminate_seen: bool,
}

#[derive(Debug, Clone, Copy)]
struct RunningHarnessBehavior {
    hold_before_cleanup: bool,
    hold_after_cleanup: bool,
    hold_after_target_exited: bool,
    fail_outer_status_after_cleanup: bool,
    hold_after_completion_request: bool,
}

fn run_running_state(
    target: &mut StoppedTarget,
    control: &mut ControlChannel,
    state: &mut MonitorState,
    harness: RunningHarnessBehavior,
) -> io::Result<()> {
    let mut inputs = RunningBoundaryInputs {
        signal_sequence: 0,
        terminate_seen: false,
    };
    loop {
        if target.drain_running_wait_events()? {
            let deadline = Instant::now() + DESCRIBE_TIMEOUT;
            check_running_boundary_inputs(
                target,
                control,
                &mut inputs.signal_sequence,
                &mut inputs.terminate_seen,
                false,
                deadline,
            )?;
            target.kill_and_reap_all_until(deadline)?;
            return finish_running_target_until(
                target,
                control,
                state,
                &mut inputs,
                harness,
                deadline,
            );
        }

        let mut pollfds = [
            libc::pollfd {
                fd: control.fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: SIGNAL_RELAY_FD,
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
        ];
        let polled = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, 10) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if pollfds[0].revents != 0 {
            let deadline = Instant::now() + DESCRIBE_TIMEOUT;
            let frame = control.receive_with_deadline(Some(deadline))?;
            if frame.kind != FrameKind::Terminate || !frame.payload.is_empty() {
                return Err(invalid_data(
                    "running target accepts only an empty authenticated terminate request",
                ));
            }
            inputs.terminate_seen = true;
            check_running_boundary_inputs(
                target,
                control,
                &mut inputs.signal_sequence,
                &mut inputs.terminate_seen,
                true,
                deadline,
            )?;
            if harness.hold_before_cleanup {
                thread::sleep(Duration::from_secs(1));
                ensure_before_deadline(
                    deadline,
                    "timed out reaping all sandbox target descendants",
                )?;
            }
            target.kill_and_reap_all_until(deadline)?;
            return finish_running_target_until(
                target,
                control,
                state,
                &mut inputs,
                harness,
                deadline,
            );
        }
        if pollfds[1].revents != 0 {
            drain_running_signal_records(
                target,
                &mut inputs.signal_sequence,
                true,
                Instant::now() + DESCRIBE_TIMEOUT,
            )?;
        }
    }
}

fn check_running_boundary_inputs(
    target: &mut StoppedTarget,
    control: &mut ControlChannel,
    signal_sequence: &mut u64,
    terminate_seen: &mut bool,
    forward_signals: bool,
    deadline: Instant,
) -> io::Result<()> {
    for _ in 0..MAX_RUNNING_EVENTS_PER_TURN {
        ensure_before_deadline(deadline, "sandbox runtime boundary deadline elapsed")?;
        let mut observed = false;
        if control_packet_available(control.fd.as_raw_fd())? {
            observed = true;
            let frame = control.receive_with_deadline(Some(deadline))?;
            if frame.kind != FrameKind::Terminate || !frame.payload.is_empty() {
                return Err(invalid_data(
                    "running target accepts only an empty authenticated terminate request",
                ));
            }
            if *terminate_seen {
                return Err(invalid_data(
                    "running target received multiple terminate requests",
                ));
            }
            *terminate_seen = true;
        }
        if signal_relay_packet_available(SIGNAL_RELAY_FD)? {
            observed = true;
            drain_running_signal_records(target, signal_sequence, forward_signals, deadline)?;
        }
        if !observed {
            ensure_before_deadline(deadline, "sandbox runtime boundary deadline elapsed")?;
            return Ok(());
        }
    }
    Err(invalid_data(
        "sandbox runtime boundary input budget was exceeded",
    ))
}

fn drain_running_signal_records(
    target: &mut StoppedTarget,
    expected_sequence: &mut u64,
    forward: bool,
    deadline: Instant,
) -> io::Result<()> {
    for _ in 0..MAX_RUNNING_EVENTS_PER_TURN {
        let Some(record) =
            receive_signal_relay_record(SIGNAL_RELAY_FD, *expected_sequence, deadline)?
        else {
            return Ok(());
        };
        *expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("sandbox signal relay sequence overflow"))?;
        if forward {
            target.forward_running_signal(record.signal)?;
        }
        if !signal_relay_packet_available(SIGNAL_RELAY_FD)? {
            return Ok(());
        }
    }
    Ok(())
}

fn finish_running_target_until(
    target: &mut StoppedTarget,
    control: &mut ControlChannel,
    state: &mut MonitorState,
    inputs: &mut RunningBoundaryInputs,
    harness: RunningHarnessBehavior,
    deadline: Instant,
) -> io::Result<()> {
    if harness.hold_after_cleanup {
        thread::sleep(Duration::from_secs(1));
        ensure_before_deadline(
            deadline,
            "sandbox target status publication deadline elapsed",
        )?;
    }
    check_running_boundary_inputs(
        target,
        control,
        &mut inputs.signal_sequence,
        &mut inputs.terminate_seen,
        false,
        deadline,
    )?;
    ensure_before_deadline(
        deadline,
        "sandbox target status publication deadline elapsed",
    )?;
    let raw_status = target
        .target_status
        .ok_or_else(|| invalid_data("sandbox target cleanup omitted its raw wait status"))?;
    let report = TargetExitedReport {
        raw_status,
        descendants_reaped: target.descendants_reaped,
    };
    if *state != MonitorState::TargetRunning {
        return Err(invalid_data("invalid sandbox monitor state transition"));
    }
    if !target.all_reaped {
        return Err(invalid_data(
            "sandbox target completion was attempted before ECHILD",
        ));
    }
    if harness.hold_after_target_exited {
        let payload = report.encode()?;
        control.send_with_deadline(FrameKind::TargetExited, payload, Some(deadline))?;
        // Packet publication is the linearization point. The state assignment
        // is deliberately infallible and no state-6 validation follows it.
        *state = MonitorState::TargetExitedAwaitingCompletion;
        return hold_state_7_boundary(control);
    }

    let mut challenge = [0u8; COMPLETION_CHALLENGE_LEN];
    getrandom::getrandom(&mut challenge).map_err(|error| {
        io::Error::other(format!("generating sandbox completion challenge: {error}"))
    })?;
    let attestation = CompletionAttestation { report, challenge };
    let payload = attestation.encode()?;
    // State 7 may legitimately outlive the launch handshake timeout when the
    // caller delays `wait()`. Parent death is still observed by poll/EOF; the
    // bounded deadline is installed only after CompleteSession arrives.
    set_control_timeouts(control.fd.as_raw_fd(), Duration::ZERO)?;
    control.send_with_deadline(FrameKind::TargetExited, payload, Some(deadline))?;
    // As above, successful SOCK_SEQPACKET publication commits the transition.
    *state = MonitorState::TargetExitedAwaitingCompletion;
    run_session_completion_state(
        control,
        state,
        attestation,
        &mut inputs.signal_sequence,
        harness.fail_outer_status_after_cleanup,
        harness.hold_after_completion_request,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionInputPhase {
    AwaitingRequest,
    AwaitingWriteEof,
}

fn run_session_completion_state(
    control: &mut ControlChannel,
    state: &mut MonitorState,
    attestation: CompletionAttestation,
    signal_sequence: &mut u64,
    fail_outer_status_after_cleanup_for_harness: bool,
    hold_after_completion_request_for_harness: bool,
) -> io::Result<()> {
    let mut deadline = None;
    let mut phase = CompletionInputPhase::AwaitingRequest;
    let mut signal_eof = false;
    loop {
        if let Some(deadline) = deadline {
            ensure_before_deadline(deadline, "sandbox completion deadline elapsed")?;
        }
        let timeout = deadline.map_or(-1, |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining
                .as_nanos()
                .saturating_add(999_999)
                .div_euclid(1_000_000)
                .clamp(1, libc::c_int::MAX as u128) as libc::c_int
        });
        let mut pollfds = [
            libc::pollfd {
                fd: control.fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: if signal_eof { -1 } else { SIGNAL_RELAY_FD },
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
        ];
        let polled = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, timeout) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if let Some(deadline) = deadline {
            ensure_before_deadline(deadline, "sandbox completion deadline elapsed")?;
        }
        if pollfds[1].revents != 0 && !signal_eof {
            let signal_deadline = deadline.unwrap_or_else(|| Instant::now() + DESCRIBE_TIMEOUT);
            drain_completion_signal_records(signal_sequence, &mut signal_eof, signal_deadline)?;
        }
        if pollfds[0].revents == 0 {
            continue;
        }

        match control.receive_with_deadline(deadline) {
            Ok(frame) => match phase {
                CompletionInputPhase::AwaitingRequest => {
                    if frame.kind != FrameKind::CompleteSession {
                        return Err(invalid_data(format!(
                            "sandbox completion received unexpected {:?} frame",
                            frame.kind
                        )));
                    }
                    attestation.require_exact_echo(&frame.payload)?;
                    phase = CompletionInputPhase::AwaitingWriteEof;
                    deadline = Some(Instant::now() + DESCRIBE_TIMEOUT);
                    if hold_after_completion_request_for_harness {
                        thread::sleep(DESCRIBE_TIMEOUT + Duration::from_millis(250));
                    }
                }
                CompletionInputPhase::AwaitingWriteEof => {
                    return Err(invalid_data(format!(
                        "sandbox completion received an additional {:?} frame before EOF",
                        frame.kind
                    )));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => match phase {
                CompletionInputPhase::AwaitingRequest => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "sandbox completion control closed before its request",
                    ));
                }
                CompletionInputPhase::AwaitingWriteEof => break,
            },
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("sandbox completion control input failed: {error}"),
                ));
            }
        }
    }

    // There is no cross-descriptor atomic primitive. Input already observable
    // here wins; successful CleanupComplete publication below is terminal.
    if !signal_eof && signal_relay_packet_available(SIGNAL_RELAY_FD)? {
        let deadline = deadline.expect("completion request established its deadline");
        drain_completion_signal_records(signal_sequence, &mut signal_eof, deadline)?;
    }
    let deadline = deadline.expect("completion request established its deadline");
    ensure_before_deadline(deadline, "sandbox completion deadline elapsed")?;
    let payload = attestation.encode()?;
    if *state != MonitorState::TargetExitedAwaitingCompletion {
        return Err(invalid_data("invalid sandbox monitor state transition"));
    }
    *state = MonitorState::SessionCompletionAuthorized;
    control.send_with_deadline(FrameKind::CleanupComplete, payload, Some(deadline))?;
    // Packet publication is the terminal state-7 linearization point.
    *state = MonitorState::CleanupCompletePublished;
    if fail_outer_status_after_cleanup_for_harness {
        // This is intentionally after the terminal packet and deliberately
        // bypasses the Fatal path: State 8 must reject a nonzero outer status
        // even when every authenticated protocol event was otherwise valid.
        unsafe { libc::_exit(23) }
    }
    Ok(())
}

/// A signal callback can already hold its serialized writer lock when the
/// monitor publishes TargetExited. The parent disarms the relay before
/// authorizing completion, so accept only the remaining valid sequence and
/// deliberately ignore those now-obsolete signals.
fn drain_completion_signal_records(
    expected_sequence: &mut u64,
    eof: &mut bool,
    deadline: Instant,
) -> io::Result<()> {
    for _ in 0..MAX_RUNNING_EVENTS_PER_TURN {
        match receive_signal_relay_record(SIGNAL_RELAY_FD, *expected_sequence, deadline) {
            Ok(Some(_)) => {
                *expected_sequence = expected_sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("sandbox signal relay sequence overflow"))?;
            }
            Ok(None) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                *eof = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        if !signal_relay_packet_available(SIGNAL_RELAY_FD)? {
            return Ok(());
        }
    }
    Err(invalid_data(
        "sandbox completion signal relay budget was exceeded",
    ))
}

fn hold_state_7_boundary(control: &mut ControlChannel) -> io::Result<()> {
    let deadline = Instant::now() + DESCRIBE_TIMEOUT;
    loop {
        ensure_before_deadline(deadline, "state-7-not-installed: control deadline elapsed")?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining
            .as_nanos()
            .saturating_add(999_999)
            .div_euclid(1_000_000)
            .clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        let mut pollfds = [
            libc::pollfd {
                fd: control.fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: SIGNAL_RELAY_FD,
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
        ];
        let polled = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, timeout) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        ensure_before_deadline(deadline, "state-7-not-installed: control deadline elapsed")?;
        if pollfds[0].revents != 0 {
            return Err(match control.receive_with_deadline(Some(deadline)) {
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    io::Error::new(io::ErrorKind::Unsupported, "state-7-not-installed")
                }
                Err(error) => invalid_data(format!(
                    "state-7-not-installed: control input failed: {error}"
                )),
                Ok(frame) => invalid_data(format!(
                    "state-7-not-installed: unexpected {:?} frame",
                    frame.kind
                )),
            });
        }
        if pollfds[1].revents != 0 {
            return Err(invalid_data(
                "state-7-not-installed: unexpected signal relay input",
            ));
        }
    }
}

fn run_target_stopped_state(
    spec: &BootstrapSpec,
    control: &mut ControlChannel,
    state: &mut MonitorState,
) -> io::Result<()> {
    let (mut target, mut attestation) = create_stopped_target(spec)?;
    let outcome = (|| {
        getrandom::getrandom(&mut attestation.start_challenge).map_err(|error| {
            io::Error::other(format!("generating target start challenge: {error}"))
        })?;
        control.send(FrameKind::TargetStopped, attestation.encode())?;
        transition_monitor_state(
            state,
            MonitorState::MonitorReleased,
            MonitorState::TargetStopped,
        )?;

        let request = receive_control_until(control, Instant::now() + DESCRIBE_TIMEOUT)?;
        let expected_start = attestation.encode();
        if request.kind != FrameKind::StartTarget
            || !constant_time_eq(&request.payload, &expected_start)
        {
            return Err(invalid_data(
                "stopped target requires its exact authenticated start attestation",
            ));
        }
        transition_monitor_state(
            state,
            MonitorState::TargetStopped,
            MonitorState::TargetStarting,
        )?;
        // C3: fork the in-netns egress bridge HERE — AFTER the parent has attested the
        // monitor's UNIQUE stopped-target child (`read_unique_monitor_child`, which precedes
        // StartTarget), so the bridge never perturbs that count; and BEFORE the target
        // resumes and starts dialing the proxy, so the loopback listener is up in time. The
        // monitor is single-threaded (no fork follows the target fork). Best-effort: a bridge
        // that can't start leaves the child's egress fail-CLOSED (empty netns), never open —
        // any bridge process is reaped by the target-tree kill(-1) cleanup and the PID-1
        // namespace collapse.
        let _ = spawn_in_netns_bridge();
        target.resume_and_release()?;
        match target.await_exec_outcome(control, Instant::now() + DESCRIBE_TIMEOUT)? {
            TargetExecOutcome::Failed(failure) => {
                target.kill_and_reap_all()?;
                control.send(FrameKind::ExecFailed, failure.encode())?;
                transition_monitor_state(
                    state,
                    MonitorState::TargetStarting,
                    MonitorState::TargetStartFailed,
                )
            }
            TargetExecOutcome::Accepted => {
                control.send(FrameKind::ExecAccepted, Vec::new())?;
                transition_monitor_state(
                    state,
                    MonitorState::TargetStarting,
                    MonitorState::TargetRunning,
                )?;
                if spec.hold_after_exec_for_harness {
                    let deadline = Instant::now() + DESCRIBE_TIMEOUT;
                    Err(match receive_control_until(control, deadline) {
                        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                            io::Error::new(
                                io::ErrorKind::Unsupported,
                                "running-state-not-installed",
                            )
                        }
                        Err(error) if error.kind() == io::ErrorKind::TimedOut => io::Error::new(
                            io::ErrorKind::TimedOut,
                            "running-state-not-installed: control deadline elapsed",
                        ),
                        Err(error) => error,
                        Ok(frame) => invalid_data(format!(
                            "running-state-not-installed: unexpected {:?} frame",
                            frame.kind
                        )),
                    })
                } else {
                    run_running_state(
                        &mut target,
                        control,
                        state,
                        RunningHarnessBehavior {
                            hold_before_cleanup: spec.hold_before_runtime_cleanup_for_harness,
                            hold_after_cleanup: spec.hold_after_runtime_cleanup_for_harness,
                            hold_after_target_exited: spec.hold_after_target_exited_for_harness,
                            fail_outer_status_after_cleanup: spec
                                .fail_outer_status_after_cleanup_for_harness,
                            hold_after_completion_request: spec
                                .hold_after_completion_request_for_harness,
                        },
                    )
                }
            }
        }
    })();

    match outcome {
        Ok(()) => Ok(()),
        Err(error) => match target.kill_and_reap_all() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "{error}; target cleanup failed: {cleanup}"
            ))),
        },
    }
}

fn create_stopped_target(
    spec: &BootstrapSpec,
) -> io::Result<(StoppedTarget, TargetStoppedAttestation)> {
    let prepared_exec = PreparedTargetExec::new(spec)?;
    let seccomp = super::linux::build_seccomp(
        spec.network_filter,
        spec.per_host,
        spec.deny_keyring,
        spec.permit_keyring_join,
    )
    .map_err(|error| io::Error::other(format!("building target seccomp: {error}")))?;
    let (gate_reader, gate_writer) = pipe_files(false)?;
    let (error_reader, error_writer) = pipe_files(true)?;
    let gate_reader_fd = gate_reader.as_raw_fd();
    let gate_writer_fd = gate_writer.as_raw_fd();
    let error_reader_fd = error_reader.as_raw_fd();
    let error_writer_fd = error_writer.as_raw_fd();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        target_child_main(
            prepared_exec.pointers(),
            seccomp.as_deref(),
            spec.hold_before_initial_stop_for_harness,
            gate_reader_fd,
            gate_writer_fd,
            error_reader_fd,
            error_writer_fd,
        );
    }

    drop(gate_reader);
    drop(error_writer);
    let mut target = StoppedTarget {
        pid,
        gate_writer: Some(gate_writer),
        error_reader,
        target_status: None,
        descendants_reaped: 0,
        all_reaped: false,
    };
    match target.wait_for_initial_stop(spec.uses_seccomp(), gate_reader_fd, error_writer_fd) {
        Ok(attestation) => Ok((target, attestation)),
        Err(error) => match target.kill_and_reap_all() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "{error}; target cleanup failed: {cleanup}"
            ))),
        },
    }
}

fn verify_stopped_target(
    pid: libc::pid_t,
    uses_seccomp: bool,
    gate_reader_fd: RawFd,
    error_writer_fd: RawFd,
) -> io::Result<TargetStoppedAttestation> {
    if unsafe { libc::getpgid(pid) } != pid || unsafe { libc::getsid(pid) } != 1 {
        return Err(invalid_data(
            "stopped target is not its own group in the monitor session",
        ));
    }
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
    };
    let pid_text = pid.to_string();
    if field("PPid:") != Some("1")
        || !field("NSpid:").is_some_and(|value| {
            value
                .split_ascii_whitespace()
                .next_back()
                .is_some_and(|value| value == pid_text.as_str())
        })
        || field("TracerPid:") != Some("0")
        || !valid_no_new_privs_status(field("NoNewPrivs:"))
    {
        return Err(invalid_data("stopped target namespace identity is invalid"));
    }
    for capability in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        if field(capability) != Some("0000000000000000") {
            return Err(invalid_data("stopped target retained capabilities"));
        }
    }
    if uses_seccomp && field("Seccomp:") != Some("2") {
        return Err(invalid_data(
            "stopped target did not install its required seccomp filter",
        ));
    }

    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| invalid_data("stopped target stat record is malformed"))?;
    let fields = stat[close + 2..].split_whitespace().collect::<Vec<_>>();
    if fields.first().copied() != Some("T")
        || fields.get(1).copied() != Some("1")
        || fields.get(2).copied() != Some(pid_text.as_str())
        || fields.get(3).copied() != Some("1")
    {
        return Err(invalid_data("stopped target process shape is invalid"));
    }
    let starttime = fields
        .get(19)
        .ok_or_else(|| invalid_data("stopped target stat omits starttime"))?
        .parse::<u64>()
        .map_err(|_| invalid_data("stopped target starttime is invalid"))?;
    if starttime == 0 {
        return Err(invalid_data("stopped target starttime is zero"));
    }

    let expected_fds = BTreeSet::from([
        libc::STDIN_FILENO,
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
        gate_reader_fd,
        error_writer_fd,
    ]);
    let actual_fds = fs::read_dir(format!("/proc/{pid}/fd"))?
        .map(|entry| {
            entry?
                .file_name()
                .to_str()
                .ok_or_else(|| invalid_data("target descriptor name is not numeric"))?
                .parse::<RawFd>()
                .map_err(|_| invalid_data("target descriptor name is not numeric"))
        })
        .collect::<io::Result<BTreeSet<_>>>()?;
    if actual_fds != expected_fds {
        return Err(invalid_data(
            "stopped target inherited an unexpected descriptor",
        ));
    }

    Ok(TargetStoppedAttestation {
        namespace_pid: u32::try_from(pid)
            .map_err(|_| invalid_data("stopped target PID is out of range"))?,
        starttime,
        start_challenge: [0; START_CHALLENGE_LEN],
    })
}

fn read_target_setup_error(fd: RawFd, status: libc::c_int) -> io::Error {
    let mut record = [0u8; TARGET_SETUP_ERROR_LEN + 1];
    let mut offset = 0usize;
    loop {
        let read = unsafe {
            libc::read(
                fd,
                record[offset..].as_mut_ptr().cast(),
                record.len() - offset,
            )
        };
        if read > 0 {
            offset += read as usize;
            if offset == record.len() {
                return invalid_data("target setup error record exceeded its budget");
            }
            continue;
        }
        if read == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return error;
        }
    }
    if offset == TARGET_SETUP_ERROR_LEN {
        return match decode_target_setup_record(&record[..offset]) {
            Ok((stage, errno)) => io::Error::other(format!(
                "target setup failed at {}: {}",
                stage.label(),
                io::Error::from_raw_os_error(errno)
            )),
            Err(error) => error,
        };
    }
    if offset != 0 {
        return invalid_data("target setup error record is truncated");
    }
    if libc::WIFSIGNALED(status) {
        return io::Error::other(format!(
            "target exited during trusted setup from signal {}",
            libc::WTERMSIG(status)
        ));
    }
    io::Error::other(format!(
        "target exited during trusted setup with status {}",
        libc::WEXITSTATUS(status)
    ))
}

fn decode_target_setup_record(record: &[u8]) -> io::Result<(TargetSetupStage, libc::c_int)> {
    if record.len() != TARGET_SETUP_ERROR_LEN || &record[..4] != TARGET_SETUP_ERROR_MAGIC {
        return Err(invalid_data("target setup error record is malformed"));
    }
    let stage = TargetSetupStage::from_raw(u16::from_le_bytes([record[4], record[5]]))
        .ok_or_else(|| invalid_data("target setup error stage is invalid"))?;
    let reserved = u16::from_le_bytes([record[6], record[7]]);
    let errno = libc::c_int::from_le_bytes(
        record[8..12]
            .try_into()
            .map_err(|_| invalid_data("target setup errno is truncated"))?,
    );
    if reserved != 0 || errno <= 0 {
        return Err(invalid_data("target setup error record is malformed"));
    }
    Ok((stage, errno))
}

fn target_child_main(
    exec: TargetExecPointers,
    seccomp: Option<&[seccompiler::sock_filter]>,
    hold_before_initial_stop_for_harness: bool,
    gate_reader_fd: RawFd,
    gate_writer_fd: RawFd,
    error_reader_fd: RawFd,
    error_writer_fd: RawFd,
) -> ! {
    if unsafe { libc::setpgid(0, 0) } != 0 {
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::ProcessGroup,
            child_errno(),
        );
    }
    if unsafe { libc::chdir(exec.cwd) } != 0 {
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::WorkingDirectory,
            child_errno(),
        );
    }
    // Every pointer consumed after the gate was built before fork.  The child
    // performs no allocation or runtime initialization before raw execve.
    // The monitor is deliberately non-dumpable, but plain Node targets are not.
    // Restore that ordinary target posture so the trusted PID-1 parent can inspect
    // the stopped child's descriptor table without weakening the monitor itself.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 1
    {
        target_child_fail(error_writer_fd, TargetSetupStage::Dumpable, child_errno());
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1
    {
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::NoNewPrivileges,
            child_errno(),
        );
    }
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::ParentDeath,
            child_errno(),
        );
    }
    if unsafe { libc::getppid() } != 1 {
        target_child_fail(error_writer_fd, TargetSetupStage::ParentDeath, libc::ECHILD);
    }
    if let Err(error) = seccomp.map(install_target_seccomp).transpose() {
        target_child_fail(error_writer_fd, TargetSetupStage::Seccomp, error);
    }
    if unsafe { child_close_all_except([gate_reader_fd, error_writer_fd]) }.is_err() {
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::DescriptorSweep,
            child_errno(),
        );
    }
    let _ = gate_writer_fd;
    let _ = error_reader_fd;
    if hold_before_initial_stop_for_harness {
        loop {
            unsafe { libc::pause() };
        }
    }
    if unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) } != 0 {
        target_child_fail(error_writer_fd, TargetSetupStage::Stop, child_errno());
    }

    let mut byte = 0u8;
    loop {
        let read = unsafe { libc::read(gate_reader_fd, (&mut byte as *mut u8).cast(), 1) };
        if read < 0 && child_errno() == libc::EINTR {
            continue;
        }
        if read == 1 && byte == START_GATE_BYTE {
            break;
        }
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::StartGate,
            if read < 0 {
                child_errno()
            } else {
                libc::EPROTO
            },
        );
    }
    loop {
        let read = unsafe { libc::read(gate_reader_fd, (&mut byte as *mut u8).cast(), 1) };
        if read < 0 && child_errno() == libc::EINTR {
            continue;
        }
        if read == 0 {
            break;
        }
        target_child_fail(
            error_writer_fd,
            TargetSetupStage::StartGate,
            if read < 0 {
                child_errno()
            } else {
                libc::EPROTO
            },
        );
    }
    if unsafe { libc::close(gate_reader_fd) } != 0 {
        target_child_fail(error_writer_fd, TargetSetupStage::StartGate, child_errno());
    }
    unsafe { libc::execve(exec.program, exec.argv, exec.envp) };
    target_child_fail(error_writer_fd, TargetSetupStage::Execve, child_errno());
}

pub(super) fn install_target_seccomp(
    program: &[seccompiler::sock_filter],
) -> Result<(), libc::c_int> {
    let len = u16::try_from(program.len()).map_err(|_| libc::E2BIG)?;
    let filter = libc::sock_fprog {
        len,
        filter: program.as_ptr().cast::<libc::sock_filter>().cast_mut(),
    };
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &filter as *const libc::sock_fprog,
            0,
            0,
        )
    } != 0
    {
        return Err(child_errno());
    }
    Ok(())
}

fn child_errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

fn encode_target_setup_record(
    stage: TargetSetupStage,
    errno: libc::c_int,
) -> [u8; TARGET_SETUP_ERROR_LEN] {
    let mut record = [0u8; TARGET_SETUP_ERROR_LEN];
    record[..4].copy_from_slice(TARGET_SETUP_ERROR_MAGIC);
    record[4..6].copy_from_slice(&(stage as u16).to_le_bytes());
    record[8..12].copy_from_slice(&errno.max(1).to_le_bytes());
    record
}

fn target_child_fail(error_writer_fd: RawFd, stage: TargetSetupStage, errno: libc::c_int) -> ! {
    let record = encode_target_setup_record(stage, errno);
    let mut offset = 0usize;
    while offset < record.len() {
        let written = unsafe {
            libc::write(
                error_writer_fd,
                record[offset..].as_ptr().cast(),
                record.len() - offset,
            )
        };
        if written > 0 {
            offset += written as usize;
            continue;
        }
        if written < 0 && child_errno() == libc::EINTR {
            continue;
        }
        break;
    }
    unsafe { libc::_exit(126) }
}

unsafe fn child_close_all_except(mut preserved: [RawFd; 2]) -> Result<(), libc::c_int> {
    let supported =
        unsafe { libc::syscall(libc::SYS_close_range, u32::MAX, u32::MAX, 0 as libc::c_uint) } == 0;
    if supported {
        if preserved[0] > preserved[1] {
            preserved.swap(0, 1);
        }
        let mut first = 3u32;
        for fd in preserved.iter().copied() {
            if fd < 3 {
                continue;
            }
            let fd = fd as u32;
            if first < fd && unsafe { libc::syscall(libc::SYS_close_range, first, fd - 1, 0) } != 0
            {
                return Err(child_errno());
            }
            first = fd.saturating_add(1);
        }
        if first < u32::MAX
            && unsafe { libc::syscall(libc::SYS_close_range, first, u32::MAX, 0) } != 0
        {
            return Err(child_errno());
        }
        return Ok(());
    }
    if child_errno() != libc::ENOSYS {
        return Err(child_errno());
    }
    unsafe { child_close_open_fds_from_proc(&preserved) }
}

unsafe fn child_close_open_fds_from_proc(preserved: &[RawFd]) -> Result<(), libc::c_int> {
    const PROC_SUPER_MAGIC: libc::c_long = 0x9fa0;
    const DIRENT_HEADER: usize = 19;
    let directory = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        ) as RawFd
    };
    if directory < 0 {
        return Err(child_errno());
    }
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory, stat.as_mut_ptr()) } != 0 {
        let error = child_errno();
        unsafe { libc::close(directory) };
        return Err(error);
    }
    if unsafe { stat.assume_init() }.f_type != PROC_SUPER_MAGIC {
        unsafe { libc::close(directory) };
        return Err(libc::EINVAL);
    }

    let mut buffer = [0u8; 8192];
    let mut saw_directory = false;
    loop {
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
            ) as isize
        };
        if count < 0 {
            let error = child_errno();
            if error == libc::EINTR {
                continue;
            }
            unsafe { libc::close(directory) };
            return Err(error);
        }
        if count == 0 {
            break;
        }
        let count = count as usize;
        let mut offset = 0usize;
        while offset < count {
            if count - offset < DIRENT_HEADER {
                unsafe { libc::close(directory) };
                return Err(libc::EIO);
            }
            let record = &buffer[offset..count];
            let reclen = u16::from_ne_bytes([record[16], record[17]]) as usize;
            if reclen < DIRENT_HEADER || offset + reclen > count {
                unsafe { libc::close(directory) };
                return Err(libc::EIO);
            }
            let name = &record[DIRENT_HEADER..reclen];
            let Some(end) = name.iter().position(|byte| *byte == 0) else {
                unsafe { libc::close(directory) };
                return Err(libc::EIO);
            };
            let name = &name[..end];
            if name != b"." && name != b".." {
                let mut fd = 0i32;
                if name.is_empty() || name.iter().any(|byte| !byte.is_ascii_digit()) {
                    unsafe { libc::close(directory) };
                    return Err(libc::EIO);
                }
                for byte in name {
                    fd = fd
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(i32::from(*byte - b'0')))
                        .ok_or(libc::EOVERFLOW)?;
                }
                if fd == directory {
                    saw_directory = true;
                } else if fd >= 3 && !preserved.contains(&fd) && unsafe { libc::close(fd) } != 0 {
                    let error = child_errno();
                    if error != libc::EBADF {
                        unsafe { libc::close(directory) };
                        return Err(error);
                    }
                }
            }
            offset += reclen;
        }
    }
    unsafe { libc::close(directory) };
    if !saw_directory {
        return Err(libc::EIO);
    }
    Ok(())
}

/// C3: fork the IN-NETNS half of the per-host egress bridge. It exists ONLY when per-host is
/// active — signalled by the presence of the relay socket, a nub-private READ-ONLY bind mount
/// the untrusted child cannot forge, probed here BEFORE the target is released. The bridge
/// runs as a hardened PROCESS (not a monitor thread): a process keeps the single-threaded
/// monitor's subsequent target fork fork-safe, and it can never be orphaned — the monitor's
/// PID-1 namespace collapse and its target-tree `kill(-1)` cleanup both reap it. `Ok(None)`
/// means "not per-host"; a fork/setup failure leaves the child's egress fail-CLOSED (the
/// empty netns has no route out), never open, so it is non-fatal to the launch.
///
/// READINESS HANDSHAKE. The caller releases the target immediately after this returns, and
/// the target dials the bridge's fixed loopback port (`HTTP_PROXY`) as soon as it runs — so
/// if this returned before the forked child had actually bound that port, the target's first
/// connect would race the child's fork/harden/bind and lose under scheduling contention
/// (observed empirically: the child's harden step + bind is fast but not instant, and a
/// loaded host reorders it after the resumed target). A pipe closes that window: the child
/// signals (one byte) right after a successful bind, or falls silent and its exit closes the
/// pipe (EOF) on any setup failure; either unblocks this read. Bounded by a short deadline so
/// a wedged child can't hang the launch — on timeout the launch still proceeds best-effort
/// (egress stays fail-CLOSED regardless: a non-listening bridge means the empty netns has no
/// route out; "proceed" is about launch-liveness, never about opening egress).
fn spawn_in_netns_bridge() -> io::Result<Option<libc::pid_t>> {
    let sock = Path::new(super::linux_net_bridge::PRIVATE_NET_ROOT)
        .join(super::linux_net_bridge::SOCK_NAME);
    if !sock.exists() {
        return Ok(None);
    }
    let mut ready_pipe = [0 as RawFd; 2];
    if unsafe { libc::pipe(ready_pipe.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let [ready_read, ready_write] = ready_pipe;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(ready_read);
            libc::close(ready_write);
        }
        return Err(error);
    }
    if pid == 0 {
        unsafe { libc::close(ready_read) };
        let code = i32::from(run_in_netns_bridge(&sock, ready_write).is_err());
        unsafe { libc::_exit(code) };
    }
    unsafe { libc::close(ready_write) };
    wait_for_bridge_ready(ready_read);
    unsafe { libc::close(ready_read) };
    Ok(Some(pid))
}

/// Block until the in-netns bridge child signals it has bound its loopback listener (one
/// byte), its pipe closes (EOF — setup failed and the process exited), or a short deadline
/// passes. Never returns an error: every outcome here is "proceed with the launch" (see the
/// best-effort contract on [`spawn_in_netns_bridge`]) — a poll/read error, a timeout, and EOF are all
/// treated the same as a positive signal so a bridge-startup hiccup can never hang the
/// target's launch.
fn wait_for_bridge_ready(read_fd: RawFd) {
    const BRIDGE_READY_TIMEOUT: Duration = Duration::from_secs(2);
    let deadline = Instant::now() + BRIDGE_READY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let timeout = remaining.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        let mut pollfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if polled < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if polled == 0 {
            return; // deadline elapsed
        }
        let mut byte = [0u8; 1];
        let read = unsafe { libc::read(read_fd, byte.as_mut_ptr().cast(), 1) };
        // A ready byte, EOF (child closed/exited), or a transient error all mean "stop
        // waiting" — only EINTR loops.
        if read >= 0 {
            return;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// The forked in-netns bridge body: harden, bind, signal readiness, then blind-pump every
/// child loopback connection to the host bridge's UDS. Never returns on success (the accept
/// loop runs for the target's whole life); the process is killed at cleanup.
fn run_in_netns_bridge(sock: &Path, ready_write: RawFd) -> io::Result<()> {
    harden_bridge_child(ready_write)?;
    // The empty netns's loopback is brought up by Bubblewrap's own `--unshare-net`
    // loopback_setup, so a fixed high port binds with no capability.
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, IN_NETNS_PROXY_PORT))?;
    // Signal the monitor that the loopback port is bound and accepting — see the readiness
    // handshake note on `spawn_in_netns_bridge`. Best-effort: the monitor also proceeds on
    // EOF (this process exiting), so a failed write here just falls back to that.
    let _ = unsafe { libc::write(ready_write, [1u8].as_ptr().cast(), 1) };
    unsafe { libc::close(ready_write) };
    let sock = sock.to_path_buf();
    for conn in listener.incoming() {
        let Ok(tcp) = conn else { continue };
        let sock = sock.clone();
        let _ = std::thread::Builder::new()
            .name("nub-net-inbridge-conn".into())
            .spawn(move || {
                if let Ok(uds) = std::os::unix::net::UnixStream::connect(&sock) {
                    super::linux_net_bridge::splice_unix_tcp(uds, tcp);
                }
            });
    }
    Ok(())
}

/// Harden the forked bridge process (mirrors Codex's `harden_bridge_process`): die with the
/// monitor (`PR_SET_PDEATHSIG`, belt-and-suspenders to the PID-ns collapse), never dumpable,
/// and drop every inherited monitor descriptor (the control/release/relay channels) so the
/// bridge cannot hold the control channel open — only stdio and the readiness pipe's write
/// end (closed by `run_in_netns_bridge` once it has signalled) are retained.
fn harden_bridge_child(ready_write: RawFd) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    close_all_except(&[
        libc::STDIN_FILENO,
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
        ready_write,
    ])
}

fn harden_monitor_before_secrets() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    normalize_signal_dispositions()?;
    // Stock Bubblewrap sets PWD=/ even after --clearenv.  The monitor does not
    // consume ambient configuration, so erase that implementation-added value
    // (and any unexpected inherited value) before reading bootstrap secrets.
    if unsafe { libc::clearenv() } != 0 {
        return Err(io::Error::last_os_error());
    }
    if std::env::vars_os().next().is_some() {
        return Err(invalid_data("sandbox monitor environment is not empty"));
    }
    if unsafe { libc::getpid() } != 1 || unsafe { libc::getppid() } != 0 {
        return Err(invalid_data("sandbox monitor is not PID-namespace PID 1"));
    }
    if unsafe { libc::getsid(0) } != 1 || unsafe { libc::getpgid(0) } != 1 {
        return Err(invalid_data(
            "sandbox monitor is not its namespace session/group leader",
        ));
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1
    {
        return Err(invalid_data("sandbox monitor hardening did not persist"));
    }
    verify_zero_capabilities()?;
    for namespace in ["user", "mnt", "pid", "ipc", "net"] {
        let value = fs::read_link(format!("/proc/self/ns/{namespace}"))?;
        if !value
            .as_os_str()
            .as_bytes()
            .starts_with(format!("{namespace}:[").as_bytes())
        {
            return Err(invalid_data("sandbox monitor namespace shape is invalid"));
        }
    }
    Ok(())
}

fn normalize_signal_dispositions() -> io::Result<()> {
    let mut blocked = unsafe { MaybeUninit::<libc::sigset_t>::zeroed().assume_init() };
    if unsafe { libc::sigfillset(&mut blocked) } != 0
        || unsafe { libc::sigprocmask(libc::SIG_SETMASK, &blocked, std::ptr::null_mut()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut action = unsafe { MaybeUninit::<libc::sigaction>::zeroed().assume_init() };
    action.sa_sigaction = libc::SIG_DFL;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for signal in 1..LINUX_NSIG {
        if signal == libc::SIGKILL || signal == libc::SIGSTOP {
            continue;
        }
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) {
                return Err(error);
            }
        }
    }
    let mut empty = unsafe { MaybeUninit::<libc::sigset_t>::zeroed().assume_init() };
    if unsafe { libc::sigemptyset(&mut empty) } != 0
        || unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn verify_zero_capabilities() -> io::Result<()> {
    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    let mut header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if data
        .iter()
        .any(|word| word.effective != 0 || word.permitted != 0 || word.inheritable != 0)
    {
        return Err(invalid_data("sandbox monitor retained Linux capabilities"));
    }
    for capability in 0..64 {
        let bounded = unsafe { libc::prctl(libc::PR_CAPBSET_READ, capability, 0, 0, 0) };
        if bounded < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            return Err(error);
        }
        if bounded != 0 {
            return Err(invalid_data(
                "sandbox monitor retained a bounding-set capability",
            ));
        }
        let ambient = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_IS_SET,
                capability,
                0,
                0,
            )
        };
        if ambient < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                continue;
            }
            return Err(error);
        }
        if ambient != 0 {
            return Err(invalid_data(
                "sandbox monitor retained an ambient capability",
            ));
        }
    }
    Ok(())
}

fn read_sealed_bootstrap(fd: RawFd) -> io::Result<BootstrapSpec> {
    validate_fd_kind(
        fd,
        libc::S_IFREG,
        "bootstrap descriptor is not a regular file",
    )?;
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(io::Error::last_os_error());
    }
    if seals & REQUIRED_BOOTSTRAP_SEALS != REQUIRED_BOOTSTRAP_SEALS {
        return Err(invalid_data(
            "sandbox monitor bootstrap is not fully sealed",
        ));
    }
    let stat = fstat(fd)?;
    let size = usize::try_from(stat.st_size)
        .map_err(|_| invalid_data("sandbox monitor bootstrap size is invalid"))?;
    if size == 0 || size > MAX_BOOTSTRAP_BYTES {
        return Err(invalid_data(
            "sandbox monitor bootstrap exceeds the byte budget",
        ));
    }
    let mut bytes = vec![0u8; size];
    let mut offset = 0;
    while offset < bytes.len() {
        let read = unsafe {
            libc::pread(
                fd,
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
                offset as libc::off_t,
            )
        };
        if read > 0 {
            offset += read as usize;
            continue;
        }
        if read == 0 {
            return Err(invalid_data("truncated sandbox monitor bootstrap"));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    BootstrapSpec::decode(&bytes)
}

fn verify_monitor_runtime_identity(spec: &BootstrapSpec) -> io::Result<[u8; 32]> {
    let executable = File::open("/proc/self/exe")?;
    let actual_executable = FileIdentity::from_file(&executable)?;
    let private_loader = Path::new(PRIVATE_RUNTIME_ROOT).join("ld.so");
    let expected_kernel_executable = spec
        .runtime_objects
        .iter()
        .find(|object| object.path == private_loader)
        .map_or(spec.executable, |object| object.identity);
    if actual_executable != expected_kernel_executable {
        return Err(invalid_data(
            "sandbox monitor kernel executable identity does not match its pinned launch image",
        ));
    }
    let private_executable = Path::new(PRIVATE_RUNTIME_ROOT).join("nub-monitor");
    if !spec
        .runtime_objects
        .iter()
        .any(|object| object.path == private_executable && object.identity == spec.executable)
    {
        return Err(invalid_data(
            "sandbox monitor bootstrap omitted its private executable identity",
        ));
    }
    let mut verified_objects = Vec::with_capacity(spec.runtime_objects.len());
    for object in &spec.runtime_objects {
        let metadata = fs::symlink_metadata(&object.path)?;
        if !metadata.file_type().is_file() {
            return Err(invalid_data(
                "sandbox monitor private runtime object is not a regular file",
            ));
        }
        let file = File::open(&object.path)?;
        let identity = FileIdentity::from_file(&file)?;
        if identity != object.identity {
            return Err(invalid_data(
                "sandbox monitor private runtime identity mismatch",
            ));
        }
        verified_objects.push(RuntimeObject {
            path: object.path.clone(),
            identity,
        });
    }
    validate_runtime_build_marker(&verified_objects, &spec.build_marker)
}

fn validate_pipe_endpoint(
    fd: RawFd,
    nonblocking: bool,
    packet_mode: bool,
    label: &str,
) -> io::Result<()> {
    validate_fd_kind(
        fd,
        libc::S_IFIFO,
        &format!("sandbox monitor {label} is not a pipe"),
    )?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_ACCMODE != libc::O_RDONLY
        || (flags & libc::O_NONBLOCK != 0) != nonblocking
        || (flags & libc::O_DIRECT != 0) != packet_mode
    {
        return Err(invalid_data(format!(
            "sandbox monitor {label} has invalid access/status flags"
        )));
    }
    Ok(())
}

fn validate_fd_kind(fd: RawFd, expected: libc::mode_t, message: &str) -> io::Result<()> {
    let stat = fstat(fd)?;
    if stat.st_mode & libc::S_IFMT != expected {
        return Err(invalid_data(message));
    }
    Ok(())
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

fn close_all_except(preserved: &[RawFd]) -> io::Result<()> {
    let descriptors = {
        let mut descriptors = Vec::new();
        for entry in fs::read_dir("/proc/self/fd")? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(fd) = name.parse::<RawFd>() {
                descriptors.push(fd);
            }
        }
        descriptors
    };
    for fd in descriptors {
        if !preserved.contains(&fd) && unsafe { libc::close(fd) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn read_exact_capability_and_eof(fd: RawFd, expected: &[u8; 32]) -> io::Result<()> {
    let mut value = [0u8; 32];
    let mut offset = 0;
    while offset < value.len() {
        let read = unsafe {
            libc::read(
                fd,
                value[offset..].as_mut_ptr().cast(),
                value.len() - offset,
            )
        };
        if read > 0 {
            offset += read as usize;
            continue;
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sandbox monitor release capability was truncated",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if !constant_time_eq(&value, expected) {
        return Err(invalid_data("sandbox monitor release capability mismatch"));
    }
    let mut extra = 0u8;
    loop {
        let read = unsafe { libc::read(fd, (&mut extra as *mut u8).cast(), 1) };
        if read == 0 {
            return Ok(());
        }
        if read > 0 {
            return Err(invalid_data(
                "sandbox monitor release capability has trailing bytes",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn send_fatal(control: &mut ControlChannel, error: &io::Error) {
    let mut payload = error.to_string().into_bytes();
    payload.truncate(1024);
    let _ = control.send_with_deadline(
        FrameKind::Fatal,
        payload,
        Some(Instant::now() + DESCRIBE_TIMEOUT),
    );
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32_checked(out: &mut Vec<u8>, value: usize) -> io::Result<()> {
    let value = u32::try_from(value).map_err(|_| invalid_input("length exceeds u32"))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn validate_item_count(value: usize) -> io::Result<()> {
    if value > MAX_VECTOR_ITEMS {
        return Err(invalid_input("sandbox protocol item budget exceeded"));
    }
    Ok(())
}

fn put_identity(out: &mut Vec<u8>, identity: FileIdentity) {
    put_u64(out, identity.dev);
    put_u64(out, identity.ino);
    put_u64(out, identity.size);
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    let encoded_len = 4usize
        .checked_add(value.len())
        .and_then(|len| out.len().checked_add(len))
        .ok_or_else(|| invalid_input("sandbox bootstrap length overflow"))?;
    if encoded_len > MAX_BOOTSTRAP_BYTES {
        return Err(invalid_input("sandbox bootstrap exceeds the byte budget"));
    }
    put_u32_checked(out, value.len())?;
    out.extend_from_slice(value);
    Ok(())
}

fn put_vec_os(out: &mut Vec<u8>, values: &[OsString]) -> io::Result<()> {
    validate_item_count(values.len())?;
    put_u32_checked(out, values.len())?;
    for value in values {
        put_bytes(out, value.as_bytes())?;
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| invalid_data("sandbox protocol offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_data("truncated sandbox protocol record"))?;
        self.offset = end;
        Ok(value)
    }

    fn expect(&mut self, expected: &[u8]) -> io::Result<()> {
        if self.take(expected.len())? != expected {
            return Err(invalid_data("sandbox protocol magic mismatch"));
        }
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| invalid_data("truncated sandbox protocol array"))
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn count(&mut self) -> io::Result<usize> {
        let count = self.u32()? as usize;
        if count > MAX_VECTOR_ITEMS {
            return Err(invalid_data("sandbox protocol item budget exceeded"));
        }
        Ok(count)
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn vec_os(&mut self) -> io::Result<Vec<OsString>> {
        let count = self.count()?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(OsString::from_vec(self.bytes()?.to_vec()));
        }
        Ok(values)
    }

    fn identity(&mut self) -> io::Result<FileIdentity> {
        Ok(FileIdentity {
            dev: self.u64()?,
            ino: self.u64()?,
            size: self.u64()?,
        })
    }

    fn finish(self) -> io::Result<()> {
        if self.offset != self.bytes.len() {
            return Err(invalid_data("trailing bytes in sandbox protocol record"));
        }
        Ok(())
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("two-byte slice"))
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four-byte slice"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight-byte slice"))
}

fn le_i64(bytes: &[u8]) -> i64 {
    i64::from_le_bytes(bytes.try_into().expect("eight-byte slice"))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(crate) fn runtime_degradation(error: io::Error) -> Degradation {
    Degradation {
        lost: vec!["process-isolation".to_string()],
        reason: Some(format!(
            "preparing the pinned sandbox monitor runtime: {error}"
        )),
    }
}

/// Harness-only real-kernel exercise for retained-monitor states 1-5. This is
/// intentionally separate from the production launcher until runtime
/// supervision and the remaining session-lifecycle states are installed.
#[doc(hidden)]
pub fn exercise_monitor_states_1_to_5(
    runtime: &RuntimeCapability,
    bwrap: impl AsRef<Path>,
) -> io::Result<()> {
    let bwrap = bwrap.as_ref();
    for case in [
        State5HarnessCase::ExecAccepted {
            network_filter: false,
        },
        State5HarnessCase::ExecAccepted {
            network_filter: true,
        },
        State5HarnessCase::ExecAcceptedDescendants,
        State5HarnessCase::AcceptedDeadline,
        State5HarnessCase::InitialStopDeadline,
        State5HarnessCase::ExecAcceptanceDeadline,
        State5HarnessCase::ExecFailure {
            program: "/state-5-missing",
            errno: libc::ENOENT,
        },
        State5HarnessCase::ExecFailure {
            program: "/proc/1/status",
            errno: libc::EACCES,
        },
        State5HarnessCase::ExecFailureReplay,
        State5HarnessCase::ExecFailureControlEof,
        State5HarnessCase::ExecRecordBadMagic,
        State5HarnessCase::ExecRecordTruncated,
        State5HarnessCase::ExecRecordTrailing,
        State5HarnessCase::ExecRecordWrongStage,
        State5HarnessCase::CloseDuringTargetStart,
        State5HarnessCase::CloseAtTargetStop,
        State5HarnessCase::EarlyStart,
        State5HarnessCase::WrongStart,
        State5HarnessCase::ReplayStart,
        State5HarnessCase::ExitRace,
        State5HarnessCase::SignalRace,
        State5HarnessCase::StopRace,
    ] {
        exercise_monitor_harness_case(runtime, bwrap, case).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("state-5 harness case {case:?}: {error}"),
            )
        })?;
    }
    Ok(())
}

/// Harness-only real-kernel exercise for retained-monitor state 6. State 7 is
/// independently selectable and cannot alter this sealed regression path.
#[doc(hidden)]
pub fn exercise_monitor_state_6(
    runtime: &RuntimeCapability,
    bwrap: impl AsRef<Path>,
) -> io::Result<()> {
    let bwrap = bwrap.as_ref();
    for case in [
        State5HarnessCase::RuntimeLiteralExit143,
        State5HarnessCase::RuntimeDefaultTerm,
        State5HarnessCase::RuntimeCountInt,
        State5HarnessCase::RuntimeCountTerm,
        State5HarnessCase::RuntimeDescendants,
        State5HarnessCase::RuntimeStopContinue,
        State5HarnessCase::RuntimeTerminate,
        State5HarnessCase::RuntimeTerminateForwardsQueuedSignal,
        State5HarnessCase::RuntimeSignalMalformed,
        State5HarnessCase::RuntimeSignalPartial,
        State5HarnessCase::RuntimeSignalTrailing,
        State5HarnessCase::RuntimeSignalReplay,
        State5HarnessCase::RuntimeSignalEof,
        State5HarnessCase::RuntimeControlBadPayload,
        State5HarnessCase::RuntimeControlReplay,
        State5HarnessCase::RuntimeControlEof,
        State5HarnessCase::RuntimeExitTerminateRace,
        State5HarnessCase::RuntimeTerminalFaultPrecedence,
        State5HarnessCase::RuntimeCleanupFaultPrecedence,
        State5HarnessCase::RuntimeState7SignalInput,
        State5HarnessCase::RuntimeHeldDeadline,
        State5HarnessCase::RuntimeHighDescriptorPressure,
    ] {
        exercise_monitor_harness_case(runtime, bwrap, case).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("state-6 harness case {case:?}: {error}"),
            )
        })?;
    }
    Ok(())
}

/// Harness-only real-kernel exercise for the one-shot retained-monitor state-7
/// completion handshake. State 8 is independently selectable and the production
/// launcher remains held.
#[doc(hidden)]
pub fn exercise_monitor_state_7(
    runtime: &RuntimeCapability,
    bwrap: impl AsRef<Path>,
) -> io::Result<()> {
    let bwrap = bwrap.as_ref();
    for case in [
        State5HarnessCase::CompletionLiteralExit143,
        State5HarnessCase::CompletionDefaultTerm,
        State5HarnessCase::CompletionDescendants,
        State5HarnessCase::CompletionWrongAttestation,
        State5HarnessCase::CompletionMalformed,
        State5HarnessCase::CompletionUnexpected,
        State5HarnessCase::CompletionDuplicate,
        State5HarnessCase::CompletionReplay,
        State5HarnessCase::CompletionEofBeforeRequest,
        State5HarnessCase::CompletionRelayInput,
        State5HarnessCase::CompletionRequestMayWait,
        State5HarnessCase::CompletionDeadlineAfterRequest,
        State5HarnessCase::CompletionSendFailure,
        State5HarnessCase::CompletionEarlyRequest,
    ] {
        exercise_monitor_harness_case(runtime, bwrap, case).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("state-7 harness case {case:?}: {error}"),
            )
        })?;
    }
    Ok(())
}

/// Harness-only proof for the trusted parent side of the state-7 completion
/// protocol. This closes state 8 without switching the production launcher or
/// exposing the session boundary through Prepared.
#[doc(hidden)]
pub fn exercise_monitor_state_8(
    runtime: &RuntimeCapability,
    bwrap: impl AsRef<Path>,
) -> io::Result<()> {
    let bwrap = bwrap.as_ref();
    for case in [
        State5HarnessCase::SessionLiteralExit143,
        State5HarnessCase::SessionDefaultTerm,
        State5HarnessCase::SessionDescendants,
        State5HarnessCase::SessionCancellation,
        State5HarnessCase::SessionOuterStatusMismatch,
    ] {
        exercise_monitor_harness_case(runtime, bwrap, case).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("state-8 harness case {case:?}: {error}"),
            )
        })?;
    }
    exercise_state_8_protocol_adversaries()?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum State5HarnessCase {
    ExecAccepted {
        network_filter: bool,
    },
    ExecAcceptedDescendants,
    AcceptedDeadline,
    InitialStopDeadline,
    ExecAcceptanceDeadline,
    ExecFailure {
        program: &'static str,
        errno: libc::c_int,
    },
    ExecFailureReplay,
    ExecFailureControlEof,
    ExecRecordBadMagic,
    ExecRecordTruncated,
    ExecRecordTrailing,
    ExecRecordWrongStage,
    CloseDuringTargetStart,
    CloseAtTargetStop,
    EarlyStart,
    WrongStart,
    ReplayStart,
    ExitRace,
    SignalRace,
    StopRace,
    RuntimeLiteralExit143,
    RuntimeDefaultTerm,
    RuntimeCountInt,
    RuntimeCountTerm,
    RuntimeDescendants,
    RuntimeStopContinue,
    RuntimeTerminate,
    RuntimeTerminateForwardsQueuedSignal,
    RuntimeSignalMalformed,
    RuntimeSignalPartial,
    RuntimeSignalTrailing,
    RuntimeSignalReplay,
    RuntimeSignalEof,
    RuntimeControlBadPayload,
    RuntimeControlReplay,
    RuntimeControlEof,
    RuntimeExitTerminateRace,
    RuntimeTerminalFaultPrecedence,
    RuntimeCleanupFaultPrecedence,
    RuntimeState7SignalInput,
    RuntimeHeldDeadline,
    RuntimeHighDescriptorPressure,
    CompletionLiteralExit143,
    CompletionDefaultTerm,
    CompletionDescendants,
    CompletionWrongAttestation,
    CompletionMalformed,
    CompletionUnexpected,
    CompletionDuplicate,
    CompletionReplay,
    CompletionEofBeforeRequest,
    CompletionRelayInput,
    CompletionRequestMayWait,
    CompletionDeadlineAfterRequest,
    CompletionSendFailure,
    CompletionEarlyRequest,
    SessionLiteralExit143,
    SessionDefaultTerm,
    SessionDescendants,
    SessionCancellation,
    SessionOuterStatusMismatch,
}

impl State5HarnessCase {
    fn network_filter(self) -> bool {
        matches!(
            self,
            Self::ExecAccepted {
                network_filter: true
            }
        )
    }

    fn target_probe_verb(self) -> &'static str {
        match self {
            Self::ExecAcceptedDescendants => "target-exec-descendants",
            Self::RuntimeDescendants | Self::CompletionDescendants => "target-exec-descendants",
            Self::RuntimeLiteralExit143
            | Self::RuntimeExitTerminateRace
            | Self::RuntimeTerminalFaultPrecedence
            | Self::CompletionLiteralExit143
            | Self::CompletionWrongAttestation
            | Self::CompletionMalformed
            | Self::CompletionUnexpected
            | Self::CompletionDuplicate
            | Self::CompletionReplay
            | Self::CompletionEofBeforeRequest
            | Self::CompletionRelayInput
            | Self::CompletionRequestMayWait
            | Self::CompletionDeadlineAfterRequest
            | Self::CompletionSendFailure
            | Self::CompletionEarlyRequest
            | Self::SessionLiteralExit143
            | Self::SessionCancellation
            | Self::SessionOuterStatusMismatch => "target-runtime-exit-143",
            Self::SessionDescendants => "target-exec-descendants",
            Self::RuntimeCountInt => "target-runtime-count-int",
            Self::RuntimeCountTerm => "target-runtime-count-term",
            Self::ExitRace => "target-exec-exit",
            Self::SignalRace => "target-exec-signal",
            Self::StopRace => "target-exec-stop",
            runtime if runtime.is_runtime() => "target-runtime-park",
            _ => "target-exec-probe",
        }
    }

    fn is_state_6(self) -> bool {
        matches!(
            self,
            Self::RuntimeLiteralExit143
                | Self::RuntimeDefaultTerm
                | Self::RuntimeCountInt
                | Self::RuntimeCountTerm
                | Self::RuntimeDescendants
                | Self::RuntimeStopContinue
                | Self::RuntimeTerminate
                | Self::RuntimeTerminateForwardsQueuedSignal
                | Self::RuntimeSignalMalformed
                | Self::RuntimeSignalPartial
                | Self::RuntimeSignalTrailing
                | Self::RuntimeSignalReplay
                | Self::RuntimeSignalEof
                | Self::RuntimeControlBadPayload
                | Self::RuntimeControlReplay
                | Self::RuntimeControlEof
                | Self::RuntimeExitTerminateRace
                | Self::RuntimeTerminalFaultPrecedence
                | Self::RuntimeCleanupFaultPrecedence
                | Self::RuntimeState7SignalInput
                | Self::RuntimeHeldDeadline
                | Self::RuntimeHighDescriptorPressure
        )
    }

    fn is_state_7(self) -> bool {
        matches!(
            self,
            Self::CompletionLiteralExit143
                | Self::CompletionDefaultTerm
                | Self::CompletionDescendants
                | Self::CompletionWrongAttestation
                | Self::CompletionMalformed
                | Self::CompletionUnexpected
                | Self::CompletionDuplicate
                | Self::CompletionReplay
                | Self::CompletionEofBeforeRequest
                | Self::CompletionRelayInput
                | Self::CompletionRequestMayWait
                | Self::CompletionDeadlineAfterRequest
                | Self::CompletionSendFailure
                | Self::CompletionEarlyRequest
        )
    }

    fn is_runtime(self) -> bool {
        self.is_state_6() || self.is_state_7() || self.is_state_8()
    }

    fn is_state_8(self) -> bool {
        matches!(
            self,
            Self::SessionLiteralExit143
                | Self::SessionDefaultTerm
                | Self::SessionDescendants
                | Self::SessionCancellation
                | Self::SessionOuterStatusMismatch
        )
    }
}

fn exercise_monitor_harness_case(
    runtime: &RuntimeCapability,
    bwrap: &Path,
    case: State5HarnessCase,
) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let image = runtime.materialize()?;
    let runtime_objects = runtime_objects(image)?;
    let (program, args) = match case {
        State5HarnessCase::ExecFailure { program, .. } => (OsString::from(program), Vec::new()),
        State5HarnessCase::ExecFailureReplay | State5HarnessCase::ExecFailureControlEof => {
            (OsString::from("/state-5-missing"), Vec::new())
        }
        _ => target_probe_command(image, case.target_probe_verb()),
    };
    let mut env = BTreeMap::new();
    env.insert(
        OsString::from("TARGET_EXEC_PROBE"),
        OsString::from("state5"),
    );
    let mut session = [0u8; 32];
    let mut release = [0u8; 32];
    getrandom::getrandom(&mut session)
        .map_err(|error| io::Error::other(format!("generating test session: {error}")))?;
    getrandom::getrandom(&mut release)
        .map_err(|error| io::Error::other(format!("generating test release gate: {error}")))?;
    let spec = BootstrapSpec {
        session,
        release,
        executable: image.executable.identity,
        build_marker: image.build_marker,
        runtime_objects: runtime_objects.clone(),
        program,
        args,
        cwd: PathBuf::from("/"),
        env,
        network_filter: case.network_filter(),
        per_host: false,
        deny_keyring: false,
        permit_keyring_join: false,
        hold_before_initial_stop_for_harness: matches!(
            case,
            State5HarnessCase::InitialStopDeadline
        ),
        hold_after_exec_for_harness: !case.is_runtime(),
        hold_before_runtime_cleanup_for_harness: matches!(
            case,
            State5HarnessCase::RuntimeTerminateForwardsQueuedSignal
        ),
        hold_after_runtime_cleanup_for_harness: matches!(
            case,
            State5HarnessCase::RuntimeCleanupFaultPrecedence
        ),
        hold_after_target_exited_for_harness: case.is_state_6(),
        fail_outer_status_after_cleanup_for_harness: matches!(
            case,
            State5HarnessCase::SessionOuterStatusMismatch
        ),
        hold_after_completion_request_for_harness: matches!(
            case,
            State5HarnessCase::CompletionDeadlineAfterRequest
        ),
    };
    let _descriptor_pressure = if matches!(case, State5HarnessCase::RuntimeHighDescriptorPressure) {
        Some(open_descriptor_pressure_through(BOOTSTRAP_FD - 1)?)
    } else {
        None
    };
    let bootstrap = sealed_memfd(&spec.encode()?)?;
    let (control_parent, control_child) = seqpacket_pair()?;
    set_passcred(control_parent.as_raw_fd())?;
    let (release_reader, mut release_writer) = pipe_files(false)?;
    let (signal_reader, signal_writer) = signal_relay_pipe_files()?;
    let mut signal_writer = Some(signal_writer);
    let (mut info_reader, info_writer) = pipe_files(true)?;

    // Every source used by the child remap graph is first moved above the
    // reserved destinations. This makes the subsequent dup2 sequence
    // collision-free even when ordinary allocation lands on 198..=201.
    let bootstrap = relocate_fd_at_least(bootstrap, FIRST_UNRESERVED_MONITOR_FD)?;
    let control_child = relocate_fd_at_least(control_child, FIRST_UNRESERVED_MONITOR_FD)?;
    let release_reader = relocate_file_at_least(release_reader, FIRST_UNRESERVED_MONITOR_FD)?;
    let signal_reader = relocate_file_at_least(signal_reader, FIRST_UNRESERVED_MONITOR_FD)?;
    let info_writer = relocate_file_at_least(info_writer, FIRST_UNRESERVED_MONITOR_FD)?;

    let mut command = Command::new(bwrap);
    command
        .env_clear()
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-net",
            "--as-pid-1",
            "--cap-drop",
            "ALL",
            "--tmpfs",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--dir",
            "/run",
            "--dir",
            "/dev/.nub-sandbox",
            "--tmpfs",
            PRIVATE_RUNTIME_ROOT,
        ])
        .arg("--dir")
        .arg(Path::new(PRIVATE_RUNTIME_ROOT).join("lib"))
        .arg("--info-fd")
        .arg(info_writer.as_raw_fd().to_string());
    let bindings = runtime_bindings(image)?
        .into_iter()
        .map(|file| duplicate_file_at_least(&file, FIRST_UNRESERVED_MONITOR_FD))
        .collect::<io::Result<Vec<_>>>()?;
    for (file, object) in bindings.iter().zip(&runtime_objects) {
        command
            .arg("--ro-bind-fd")
            .arg(file.as_raw_fd().to_string())
            .arg(&object.path);
    }
    command.arg("--remount-ro").arg(PRIVATE_RUNTIME_ROOT);
    command.args(["--chdir", "/", "--clearenv", "--"]);
    append_monitor_command(&mut command, image);
    command
        .arg(MONITOR_SENTINEL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    let inherited = bindings
        .iter()
        .map(AsRawFd::as_raw_fd)
        .chain(std::iter::once(info_writer.as_raw_fd()))
        .collect::<Vec<_>>();
    let remaps = [
        (bootstrap.as_raw_fd(), BOOTSTRAP_FD),
        (control_child.as_raw_fd(), CONTROL_FD),
        (release_reader.as_raw_fd(), RELEASE_FD),
        (signal_reader.as_raw_fd(), SIGNAL_RELAY_FD),
    ];
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            for &(source, destination) in &remaps {
                if source == destination {
                    clear_cloexec(destination)?;
                } else if libc::dup2(source, destination) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            for &fd in &inherited {
                clear_cloexec(fd)?;
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    drop(control_child);
    drop(release_reader);
    drop(signal_reader);
    drop(info_writer);
    let result = (|| {
        let monitor_pid = read_bwrap_child_pid(&mut info_reader, &mut child)?;
        set_control_timeouts(control_parent.as_raw_fd(), DESCRIBE_TIMEOUT)?;
        let mut control = ControlChannel::new(
            control_parent,
            session,
            ExpectedPeer {
                pid: Some(monitor_pid),
                uid: unsafe { libc::getuid() },
            },
        )?;
        let ready = control.receive()?;
        if ready.kind != FrameKind::MonitorReady || ready.payload.len() != 56 {
            return Err(invalid_data(
                "monitor did not send the exact ready attestation",
            ));
        }
        let mut cursor = Cursor::new(&ready.payload);
        if cursor.identity()? != image.executable.identity
            || cursor.array::<32>()? != image.build_marker
        {
            return Err(invalid_data("monitor ready attestation identity mismatch"));
        }
        cursor.finish()?;
        verify_monitor_host_state(monitor_pid, child.id())?;

        if unsafe { libc::kill(monitor_pid, libc::SIGSTOP) } != 0 {
            return Err(io::Error::last_os_error());
        }
        wait_for_host_process_state(monitor_pid, b'T', DESCRIBE_TIMEOUT)?;
        if unsafe { libc::kill(monitor_pid, libc::SIGCONT) } != 0 {
            return Err(io::Error::last_os_error());
        }
        wait_for_host_process_not_state(monitor_pid, b'T', DESCRIBE_TIMEOUT)?;
        thread::sleep(Duration::from_millis(100));
        if child.try_wait()?.is_some() || control_packet_available(control.fd.as_raw_fd())? {
            return Err(invalid_data(
                "hostile SIGCONT bypassed the monitor release capability",
            ));
        }

        if matches!(case, State5HarnessCase::EarlyStart) {
            control.send(FrameKind::StartTarget, Vec::new())?;
        }
        release_writer.write_all(&release)?;
        drop(release_writer);
        if matches!(case, State5HarnessCase::InitialStopDeadline) {
            set_control_timeouts(control.fd.as_raw_fd(), DESCRIBE_TIMEOUT.saturating_mul(2))?;
            let started = Instant::now();
            expect_fatal_contains(
                &mut control,
                "timed out waiting for the target's initial stop",
            )?;
            if started.elapsed() < DESCRIBE_TIMEOUT.saturating_sub(Duration::from_millis(250)) {
                return Err(invalid_data(
                    "initial target stop failed before its monotonic deadline",
                ));
            }
            wait_for_child_exit(&mut child, DESCRIBE_TIMEOUT)?;
            return Ok(());
        }
        let stopped = control.receive()?;
        if stopped.kind != FrameKind::TargetStopped {
            return Err(invalid_data(
                "monitor did not authenticate the stopped target",
            ));
        }
        let attestation = TargetStoppedAttestation::decode(&stopped.payload)?;

        // A queued pre-attestation start request is intentionally consumed as
        // soon as TargetStopped is emitted. The authenticated Fatal plus exact
        // outer-child reap is the teardown oracle.
        if matches!(case, State5HarnessCase::EarlyStart) {
            expect_fatal_contains(&mut control, "exact authenticated start attestation")?;
            wait_for_child_exit(&mut child, DESCRIBE_TIMEOUT)?;
            return Ok(());
        }

        let target_pid = read_unique_monitor_child(monitor_pid)?;
        verify_target_host_state(
            target_pid,
            monitor_pid,
            attestation,
            ExpectedTargetState::Stopped,
            case.network_filter(),
        )?;

        if child.try_wait()?.is_some() || control_packet_available(control.fd.as_raw_fd())? {
            return Err(invalid_data(
                "stopped target changed state before authenticated release",
            ));
        }

        match case {
            State5HarnessCase::CloseAtTargetStop => {
                drop(control);
            }
            State5HarnessCase::WrongStart => {
                let mut wrong = attestation.encode();
                wrong[TargetStoppedAttestation::ENCODED_LEN - 1] ^= 1;
                control.send(FrameKind::StartTarget, wrong)?;
                expect_fatal_contains(&mut control, "exact authenticated start attestation")?;
            }
            State5HarnessCase::ReplayStart => {
                let writer = duplicate_target_exec_error_writer(target_pid)?;
                control.send(FrameKind::StartTarget, attestation.encode())?;
                control.send(FrameKind::StartTarget, attestation.encode())?;
                expect_fatal_contains(
                    &mut control,
                    "unexpected StartTarget control frame during target exec",
                )?;
                drop(writer);
            }
            State5HarnessCase::ExecFailure { errno, .. } => {
                control.send(FrameKind::StartTarget, attestation.encode())?;
                let failed = control.receive()?;
                if failed.kind != FrameKind::ExecFailed {
                    return Err(invalid_data(
                        "monitor did not authenticate the target exec failure",
                    ));
                }
                let failure = TargetExecFailure::decode(&failed.payload)?;
                if failure.stage != TargetSetupStage::Execve
                    || failure.errno != errno
                    || !libc::WIFEXITED(failure.raw_status)
                    || libc::WEXITSTATUS(failure.raw_status) != 126
                {
                    return Err(invalid_data(
                        "monitor reported the wrong target exec-failure result",
                    ));
                }
                match control.receive() {
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
                    Err(error) => return Err(error),
                    Ok(_) => {
                        return Err(invalid_data(
                            "monitor sent a trailing frame after target exec failure",
                        ));
                    }
                }
            }
            State5HarnessCase::ExecFailureReplay => {
                let writer = duplicate_target_exec_error_writer(target_pid)?;
                control.send(FrameKind::StartTarget, attestation.encode())?;
                control.send(FrameKind::StartTarget, attestation.encode())?;
                expect_fatal_contains(
                    &mut control,
                    "unexpected StartTarget control frame during target exec",
                )?;
                drop(writer);
            }
            State5HarnessCase::ExecFailureControlEof => {
                let writer = duplicate_target_exec_error_writer(target_pid)?;
                control.send(FrameKind::StartTarget, attestation.encode())?;
                if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                expect_fatal_contains(&mut control, "control channel closed")?;
                drop(writer);
            }
            State5HarnessCase::ExecRecordBadMagic
            | State5HarnessCase::ExecRecordTruncated
            | State5HarnessCase::ExecRecordTrailing
            | State5HarnessCase::ExecRecordWrongStage => {
                let mut writer = duplicate_target_exec_error_writer(target_pid)?;
                let mut record = encode_target_setup_record(TargetSetupStage::Execve, libc::EIO);
                let (bytes, expected) = match case {
                    State5HarnessCase::ExecRecordBadMagic => {
                        record[0] ^= 0xff;
                        (
                            record.as_slice(),
                            &["target setup error record is malformed"] as &[&str],
                        )
                    }
                    State5HarnessCase::ExecRecordTruncated => (
                        &record[..TARGET_SETUP_ERROR_LEN - 1],
                        &["target exec-error record was truncated"] as &[&str],
                    ),
                    State5HarnessCase::ExecRecordTrailing => {
                        writer.write_all(&record)?;
                        writer.write_all(&[0])?;
                        (&[][..], &["record exceeded its fixed budget"] as &[&str])
                    }
                    State5HarnessCase::ExecRecordWrongStage => {
                        record = encode_target_setup_record(
                            TargetSetupStage::DescriptorSweep,
                            libc::EIO,
                        );
                        (
                            record.as_slice(),
                            &["target reported a non-exec setup failure"] as &[&str],
                        )
                    }
                    _ => unreachable!("record-fault cases are matched above"),
                };
                writer.write_all(bytes)?;
                drop(writer);
                control.send(FrameKind::StartTarget, attestation.encode())?;
                expect_fatal_contains_any(&mut control, expected)?;
            }
            State5HarnessCase::ExecAcceptanceDeadline => {
                let writer = duplicate_target_exec_error_writer(target_pid)?;
                set_control_timeouts(control.fd.as_raw_fd(), DESCRIBE_TIMEOUT.saturating_mul(2))?;
                let started = Instant::now();
                control.send(FrameKind::StartTarget, attestation.encode())?;
                expect_fatal_contains(&mut control, "timed out awaiting target exec acceptance")?;
                if started.elapsed() < DESCRIBE_TIMEOUT.saturating_sub(Duration::from_millis(250)) {
                    return Err(invalid_data(
                        "target exec acceptance failed before its monotonic deadline",
                    ));
                }
                drop(writer);
            }
            State5HarnessCase::CloseDuringTargetStart => {
                let writer = duplicate_target_exec_error_writer(target_pid)?;
                control.send(FrameKind::StartTarget, attestation.encode())?;
                if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                expect_fatal_contains(&mut control, "control channel closed")?;
                drop(writer);
            }
            State5HarnessCase::ExecAccepted { .. }
            | State5HarnessCase::ExecAcceptedDescendants
            | State5HarnessCase::AcceptedDeadline => {
                control.send(FrameKind::StartTarget, attestation.encode())?;
                let accepted = control.receive()?;
                if accepted.kind != FrameKind::ExecAccepted || !accepted.payload.is_empty() {
                    return Err(invalid_data(
                        "monitor did not authenticate the accepted target exec",
                    ));
                }
                verify_accepted_target(
                    target_pid,
                    monitor_pid,
                    attestation,
                    case.network_filter(),
                    &spec.program,
                    &spec.args,
                )?;
                if matches!(case, State5HarnessCase::ExecAcceptedDescendants) {
                    wait_for_runtime_ready_name(target_pid)?;
                }
                match case {
                    State5HarnessCase::AcceptedDeadline => {
                        set_control_timeouts(
                            control.fd.as_raw_fd(),
                            DESCRIBE_TIMEOUT.saturating_mul(2),
                        )?;
                        let started = Instant::now();
                        expect_fatal_contains(&mut control, "control deadline elapsed")?;
                        if started.elapsed()
                            < DESCRIBE_TIMEOUT.saturating_sub(Duration::from_millis(250))
                        {
                            return Err(invalid_data(
                                "accepted-target hold expired before its monotonic deadline",
                            ));
                        }
                    }
                    _ => {
                        if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
                            return Err(io::Error::last_os_error());
                        }
                        expect_fatal_contains(&mut control, "running-state-not-installed")?;
                    }
                }
            }
            runtime_case if runtime_case.is_runtime() => {
                exercise_monitor_runtime_case(RuntimeCaseArgs {
                    case: runtime_case,
                    control: &mut control,
                    signal_writer: &mut signal_writer,
                    child: &mut child,
                    target_pid,
                    monitor_pid,
                    attestation,
                    spec: &spec,
                })?;
            }
            State5HarnessCase::ExitRace | State5HarnessCase::SignalRace => {
                control.send(FrameKind::StartTarget, attestation.encode())?;
                match control.receive_with_deadline(Some(Instant::now() + DESCRIBE_TIMEOUT)) {
                    Ok(frame)
                        if frame.kind == FrameKind::ExecAccepted && frame.payload.is_empty() =>
                    {
                        if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
                            return Err(io::Error::last_os_error());
                        }
                        expect_fatal_contains(&mut control, "running-state-not-installed")?;
                    }
                    Ok(frame) if frame.kind == FrameKind::Fatal => {
                        let message = String::from_utf8_lossy(&frame.payload);
                        if !message.contains("target terminated without an exec-error record") {
                            return Err(invalid_data(format!(
                                "terminal exec race returned the wrong fatal result: {message}"
                            )));
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::BrokenPipe
                        ) => {}
                    Ok(frame) => {
                        return Err(invalid_data(format!(
                            "terminal exec race returned unexpected {:?}",
                            frame.kind
                        )));
                    }
                    Err(error) => return Err(error),
                }
            }
            State5HarnessCase::StopRace => {
                control.send(FrameKind::StartTarget, attestation.encode())?;
                match control.receive_with_deadline(Some(Instant::now() + DESCRIBE_TIMEOUT)) {
                    Ok(frame)
                        if frame.kind == FrameKind::ExecAccepted && frame.payload.is_empty() =>
                    {
                        expect_fatal_contains_any(
                            &mut control,
                            &[
                                "target stopped again during the exec transition",
                                "running-state-not-installed",
                            ],
                        )?;
                    }
                    Ok(frame) if frame.kind == FrameKind::Fatal => {
                        let message = String::from_utf8_lossy(&frame.payload);
                        if !message.contains("target stopped again during the exec transition")
                            && !message.contains("running-state-not-installed")
                        {
                            return Err(invalid_data(format!(
                                "stop race returned the wrong fatal result: {message}"
                            )));
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::BrokenPipe
                        ) => {}
                    Ok(frame) => {
                        return Err(invalid_data(format!(
                            "stop race returned unexpected {:?}",
                            frame.kind
                        )));
                    }
                    Err(error) => return Err(error),
                }
            }
            State5HarnessCase::EarlyStart | State5HarnessCase::InitialStopDeadline => {
                unreachable!("handled before stopped-state checks")
            }
            _ => unreachable!("runtime cases are handled by the guarded arm"),
        }
        wait_for_host_target_exit(target_pid, attestation.starttime, DESCRIBE_TIMEOUT)?;
        wait_for_child_exit(&mut child, DESCRIBE_TIMEOUT)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = wait_for_child_exit(&mut child, DESCRIBE_TIMEOUT);
    }
    result
}

/// Grouped arguments for [`exercise_monitor_runtime_case`] (pure grouping to stay under
/// clippy's arg-count lint; each field is used exactly as the corresponding parameter was).
struct RuntimeCaseArgs<'a> {
    case: State5HarnessCase,
    control: &'a mut ControlChannel,
    signal_writer: &'a mut Option<File>,
    child: &'a mut std::process::Child,
    target_pid: libc::pid_t,
    monitor_pid: libc::pid_t,
    attestation: TargetStoppedAttestation,
    spec: &'a BootstrapSpec,
}

fn exercise_monitor_runtime_case(args: RuntimeCaseArgs<'_>) -> io::Result<()> {
    let RuntimeCaseArgs {
        case,
        control,
        signal_writer,
        child,
        target_pid,
        monitor_pid,
        attestation,
        spec,
    } = args;
    control.send(FrameKind::StartTarget, attestation.encode())?;
    let accepted = control.receive()?;
    if accepted.kind != FrameKind::ExecAccepted || !accepted.payload.is_empty() {
        return Err(invalid_data(
            "monitor did not authenticate the state-6 target exec",
        ));
    }
    verify_accepted_target(
        target_pid,
        monitor_pid,
        attestation,
        false,
        &spec.program,
        &spec.args,
    )?;
    wait_for_runtime_ready_name(target_pid)?;

    if case.is_state_7() {
        return exercise_monitor_completion_case(
            case,
            control,
            signal_writer,
            child,
            target_pid,
            attestation,
        );
    }
    if case.is_state_8() {
        return exercise_monitor_parent_session_case(case, control, signal_writer, child);
    }

    let mut signal_sequence = 0u64;
    let mut expected_report = None;
    let mut expected_fatal = None;
    match case {
        State5HarnessCase::RuntimeLiteralExit143 => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            expected_report = Some(ExpectedTargetExit::Exited(143));
        }
        State5HarnessCase::RuntimeDefaultTerm
        | State5HarnessCase::RuntimeHighDescriptorPressure => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGTERM)?;
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGTERM));
        }
        State5HarnessCase::RuntimeCountInt => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGINT)?;
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            expected_report = Some(ExpectedTargetExit::Exited(41));
        }
        State5HarnessCase::RuntimeCountTerm => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGTERM)?;
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            expected_report = Some(ExpectedTargetExit::Exited(41));
        }
        State5HarnessCase::RuntimeDescendants => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGTERM)?;
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGTERM));
        }
        State5HarnessCase::RuntimeStopContinue => {
            if unsafe { libc::kill(target_pid, libc::SIGSTOP) } != 0 {
                return Err(io::Error::last_os_error());
            }
            wait_for_host_process_state(target_pid, b'T', DESCRIBE_TIMEOUT)?;
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGCONT)?;
            wait_for_host_process_not_state(target_pid, b'T', DESCRIBE_TIMEOUT)?;
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGTERM)?;
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGTERM));
        }
        State5HarnessCase::RuntimeTerminate => {
            control.send(FrameKind::Terminate, Vec::new())?;
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGKILL));
        }
        State5HarnessCase::RuntimeTerminateForwardsQueuedSignal => {
            if unsafe { libc::kill(monitor_pid, libc::SIGSTOP) } != 0 {
                return Err(io::Error::last_os_error());
            }
            wait_for_host_process_state(monitor_pid, b'T', DESCRIBE_TIMEOUT)?;
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            control.send(FrameKind::Terminate, Vec::new())?;
            if unsafe { libc::kill(monitor_pid, libc::SIGCONT) } != 0 {
                return Err(io::Error::last_os_error());
            }
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGUSR1));
        }
        State5HarnessCase::RuntimeSignalMalformed => {
            write_signal_relay_packet(signal_writer, &[0u8; SIGNAL_RELAY_RECORD_LEN])?;
            expected_fatal = Some("signal relay record is malformed");
        }
        State5HarnessCase::RuntimeSignalPartial => {
            let record = SignalRelayRecord {
                sequence: 0,
                signal: libc::SIGCONT,
            }
            .encode()?;
            write_signal_relay_packet(signal_writer, &record[..record.len() - 1])?;
            expected_fatal = Some("signal relay record is malformed");
        }
        State5HarnessCase::RuntimeSignalTrailing => {
            let record = SignalRelayRecord {
                sequence: 0,
                signal: libc::SIGCONT,
            }
            .encode()?;
            let mut trailing = record.to_vec();
            trailing.push(0);
            write_signal_relay_packet(signal_writer, &trailing)?;
            expected_fatal = Some("signal relay record is malformed");
        }
        State5HarnessCase::RuntimeSignalReplay => {
            let record = SignalRelayRecord {
                sequence: 0,
                signal: libc::SIGCONT,
            }
            .encode()?;
            write_signal_relay_packet(signal_writer, &record)?;
            write_signal_relay_packet(signal_writer, &record)?;
            expected_fatal = Some("signal relay sequence mismatch or replay");
        }
        State5HarnessCase::RuntimeSignalEof => {
            signal_writer.take();
            expected_fatal = Some("signal relay closed");
        }
        State5HarnessCase::RuntimeControlBadPayload => {
            control.send(FrameKind::Terminate, vec![1])?;
            expected_fatal = Some("empty authenticated terminate request");
        }
        State5HarnessCase::RuntimeControlReplay => {
            let replay = Frame {
                session: control.session,
                sequence: 0,
                kind: FrameKind::Terminate,
                payload: Vec::new(),
            }
            .encode()?;
            send_raw_control_packet(control.fd.as_raw_fd(), &replay)?;
            expected_fatal = Some("control sequence mismatch or replay");
        }
        State5HarnessCase::RuntimeControlEof => {
            if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
                return Err(io::Error::last_os_error());
            }
            expected_fatal = Some("control channel closed");
        }
        State5HarnessCase::RuntimeExitTerminateRace => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            control.send(FrameKind::Terminate, Vec::new())?;
            expected_report = Some(ExpectedTargetExit::ExitedOrKilled(143, libc::SIGKILL));
        }
        State5HarnessCase::RuntimeTerminalFaultPrecedence => {
            if unsafe { libc::kill(monitor_pid, libc::SIGSTOP) } != 0 {
                return Err(io::Error::last_os_error());
            }
            wait_for_host_process_state(monitor_pid, b'T', DESCRIBE_TIMEOUT)?;
            if unsafe { libc::kill(target_pid, libc::SIGUSR1) } != 0 {
                return Err(io::Error::last_os_error());
            }
            wait_for_host_process_state(target_pid, b'Z', DESCRIBE_TIMEOUT)?;
            write_signal_relay_packet(signal_writer, &[0u8; SIGNAL_RELAY_RECORD_LEN])?;
            if unsafe { libc::kill(monitor_pid, libc::SIGCONT) } != 0 {
                return Err(io::Error::last_os_error());
            }
            expected_fatal = Some("signal relay record is malformed");
        }
        State5HarnessCase::RuntimeCleanupFaultPrecedence => {
            control
                .send(FrameKind::Terminate, Vec::new())
                .map_err(|error| {
                    io::Error::new(error.kind(), format!("sending cleanup terminate: {error}"))
                })?;
            wait_for_host_target_exit(target_pid, attestation.starttime, DESCRIBE_TIMEOUT)
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("waiting for cleanup target reap: {error}"),
                    )
                })?;
            write_signal_relay_packet(signal_writer, &[0u8; SIGNAL_RELAY_RECORD_LEN]).map_err(
                |error| {
                    io::Error::new(
                        error.kind(),
                        format!("injecting cleanup relay fault: {error}"),
                    )
                },
            )?;
            expected_fatal = Some("signal relay record is malformed");
        }
        State5HarnessCase::RuntimeState7SignalInput => {
            control.send(FrameKind::Terminate, Vec::new())?;
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGKILL));
        }
        State5HarnessCase::RuntimeHeldDeadline => {
            control.send(FrameKind::Terminate, Vec::new())?;
            expected_report = Some(ExpectedTargetExit::Signaled(libc::SIGKILL));
        }
        _ => unreachable!("non-runtime case passed to state-6 harness"),
    }

    if let Some(needle) = expected_fatal {
        expect_fatal_contains(control, needle)?;
        return Ok(());
    }

    let report = expect_target_exited(control)?;
    expected_report
        .ok_or_else(|| invalid_data("state-6 case omitted its expected target result"))?
        .verify(report.raw_status)?;
    let expected_descendants = u32::from(matches!(case, State5HarnessCase::RuntimeDescendants)) * 2;
    if report.descendants_reaped != expected_descendants {
        return Err(invalid_data(format!(
            "monitor reported {} descendant reaps, expected {expected_descendants}",
            report.descendants_reaped
        )));
    }

    if matches!(case, State5HarnessCase::RuntimeHeldDeadline) {
        set_control_timeouts(control.fd.as_raw_fd(), DESCRIBE_TIMEOUT.saturating_mul(2))?;
        let started = Instant::now();
        expect_fatal_contains(control, "state-7-not-installed: control deadline elapsed")?;
        if started.elapsed() < DESCRIBE_TIMEOUT.saturating_sub(Duration::from_millis(250)) {
            return Err(invalid_data(
                "state-7 boundary failed before its monotonic deadline",
            ));
        }
    } else if matches!(case, State5HarnessCase::RuntimeState7SignalInput) {
        send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGCONT)?;
        expect_fatal_contains(
            control,
            "state-7-not-installed: unexpected signal relay input",
        )?;
    } else {
        if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
            return Err(io::Error::last_os_error());
        }
        expect_fatal_contains(control, "state-7-not-installed")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentCompletionAction {
    Complete,
    CancelBeforeRequest,
    CancelAfterRequest,
    CancelBeforeCommit,
}

#[derive(Debug)]
struct ParentCompletionResult {
    status: ExitStatus,
    report: TargetExitedReport,
}

#[derive(Debug, Clone, Copy)]
enum State8PeerCase {
    SuccessLiteral143,
    SuccessSignal,
    MalformedTargetExited,
    UnexpectedTargetFrame,
    ReplayedTargetExited,
    WrongSessionTargetExited,
    EofBeforeTargetExited,
    WrongCleanupReport,
    WrongCleanupCapability,
    UnexpectedCleanupFrame,
    ReplayedCleanup,
    ExtraAfterCleanup,
    EofBeforeCleanup,
    CancellationAfterRequest,
    CancellationBeforeCommit,
    MissingCleanupDeadline,
    LateCleanup,
    LateOuterExit,
    OuterNonzero,
    OuterSignaled,
}

impl State8PeerCase {
    fn tag(self) -> u8 {
        self as u8 + 1
    }

    fn completion_timeout(self) -> Duration {
        match self {
            Self::MissingCleanupDeadline => DESCRIBE_TIMEOUT,
            Self::LateCleanup | Self::LateOuterExit => Duration::from_millis(150),
            _ => Duration::from_secs(1),
        }
    }

    fn outer_verb(self) -> &'static str {
        match self {
            Self::SuccessLiteral143 | Self::SuccessSignal => "state-8-outer-zero",
            Self::CancellationBeforeCommit => "state-8-outer-zero",
            Self::LateOuterExit => "state-8-outer-late-zero",
            Self::OuterNonzero => "state-8-outer-23",
            Self::OuterSignaled => "state-8-outer-term",
            _ => "state-8-outer-park",
        }
    }

    fn expected_error(self) -> Option<(io::ErrorKind, &'static str)> {
        Some(match self {
            Self::SuccessLiteral143 | Self::SuccessSignal => return None,
            Self::MalformedTargetExited => (
                io::ErrorKind::InvalidData,
                "invalid sandbox completion attestation length",
            ),
            Self::UnexpectedTargetFrame => (
                io::ErrorKind::InvalidData,
                "expected TargetExited, received CleanupComplete",
            ),
            Self::ReplayedTargetExited | Self::ReplayedCleanup => (
                io::ErrorKind::InvalidData,
                "control sequence mismatch or replay",
            ),
            Self::WrongSessionTargetExited => {
                (io::ErrorKind::InvalidData, "frame session mismatch")
            }
            Self::EofBeforeTargetExited | Self::EofBeforeCleanup => {
                (io::ErrorKind::UnexpectedEof, "control channel closed")
            }
            Self::WrongCleanupReport | Self::WrongCleanupCapability => {
                (io::ErrorKind::InvalidData, "exact attestation")
            }
            Self::UnexpectedCleanupFrame => (
                io::ErrorKind::InvalidData,
                "expected CleanupComplete, received Fatal",
            ),
            Self::ExtraAfterCleanup => (
                io::ErrorKind::InvalidData,
                "unexpected post-completion Fatal",
            ),
            Self::CancellationAfterRequest | Self::CancellationBeforeCommit => (
                io::ErrorKind::Interrupted,
                "completion cancelled before result commitment",
            ),
            Self::MissingCleanupDeadline | Self::LateCleanup => {
                (io::ErrorKind::TimedOut, "control deadline elapsed")
            }
            Self::LateOuterExit => (
                io::ErrorKind::TimedOut,
                "timed out waiting for held monitor probe to exit",
            ),
            Self::OuterNonzero | Self::OuterSignaled => {
                (io::ErrorKind::InvalidData, "outer process exited")
            }
        })
    }
}

fn exercise_state_8_protocol_adversaries() -> io::Result<()> {
    for case in [
        State8PeerCase::SuccessLiteral143,
        State8PeerCase::SuccessSignal,
        State8PeerCase::MalformedTargetExited,
        State8PeerCase::UnexpectedTargetFrame,
        State8PeerCase::ReplayedTargetExited,
        State8PeerCase::WrongSessionTargetExited,
        State8PeerCase::EofBeforeTargetExited,
        State8PeerCase::WrongCleanupReport,
        State8PeerCase::WrongCleanupCapability,
        State8PeerCase::UnexpectedCleanupFrame,
        State8PeerCase::ReplayedCleanup,
        State8PeerCase::ExtraAfterCleanup,
        State8PeerCase::EofBeforeCleanup,
        State8PeerCase::CancellationAfterRequest,
        State8PeerCase::CancellationBeforeCommit,
        State8PeerCase::MissingCleanupDeadline,
        State8PeerCase::LateCleanup,
        State8PeerCase::LateOuterExit,
        State8PeerCase::OuterNonzero,
        State8PeerCase::OuterSignaled,
    ] {
        exercise_state_8_protocol_adversary(case).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("state-8 parent protocol case {case:?}: {error}"),
            )
        })?;
    }
    Ok(())
}

fn exercise_state_8_protocol_adversary(case: State8PeerCase) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let executable = std::env::current_exe()?;
    let session = [case.tag(); 32];
    let (parent_fd, peer_fd) = seqpacket_pair()?;
    let parent_pid = unsafe { libc::getpid() };
    let peer_pid = unsafe { libc::fork() };
    if peer_pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if peer_pid == 0 {
        drop(parent_fd);
        let outcome = ControlChannel::new(
            peer_fd,
            session,
            ExpectedPeer {
                pid: Some(parent_pid),
                uid: unsafe { libc::getuid() },
            },
        )
        .and_then(|mut peer| run_state_8_protocol_peer(case, &mut peer));
        unsafe { libc::_exit(if outcome.is_ok() { 0 } else { 125 }) }
    }
    drop(peer_fd);

    let mut outer = None;
    let execution = (|| -> io::Result<(io::Result<ParentCompletionResult>, libc::c_int)> {
        outer = Some(
            Command::new(&executable)
                .arg(case.outer_verb())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut control = ControlChannel::new(
            parent_fd,
            session,
            ExpectedPeer {
                pid: Some(peer_pid),
                uid: unsafe { libc::getuid() },
            },
        )?;
        let action = match case {
            State8PeerCase::CancellationAfterRequest => ParentCompletionAction::CancelAfterRequest,
            State8PeerCase::CancellationBeforeCommit => ParentCompletionAction::CancelBeforeCommit,
            _ => ParentCompletionAction::Complete,
        };
        let started = Instant::now();
        let result = drive_parent_completion_session(
            &mut control,
            outer.as_mut().expect("state-8 outer child is initialized"),
            action,
            case.completion_timeout(),
        );
        let elapsed = started.elapsed();
        drop(control);
        if case
            .expected_error()
            .is_some_and(|(kind, _)| kind == io::ErrorKind::TimedOut)
            && elapsed
                < case
                    .completion_timeout()
                    .saturating_sub(Duration::from_millis(25))
        {
            return Err(invalid_data(
                "state-8 timeout case failed before its absolute deadline",
            ));
        }
        let peer_status =
            wait_for_raw_child_status(peer_pid, DESCRIBE_TIMEOUT + Duration::from_secs(1))?;
        Ok((result, peer_status))
    })();

    let (result, peer_status) = match execution {
        Ok(value) => value,
        Err(error) => {
            if let Some(outer) = outer.as_mut() {
                let _ = fail_closed_outer(outer);
            }
            terminate_and_reap_raw_child(peer_pid);
            return Err(error);
        }
    };
    if !libc::WIFEXITED(peer_status) || libc::WEXITSTATUS(peer_status) != 0 {
        return Err(invalid_data(format!(
            "state-8 protocol peer exited with raw status {peer_status:#x}"
        )));
    }

    match case {
        State8PeerCase::SuccessLiteral143 => {
            let result = result?;
            if result.status.code() != Some(143) || result.status.signal().is_some() {
                return Err(invalid_data(
                    "state-8 literal exit 143 was not preserved as an exit code",
                ));
            }
        }
        State8PeerCase::SuccessSignal => {
            let result = result?;
            if result.status.code().is_some() || result.status.signal() != Some(libc::SIGTERM) {
                return Err(invalid_data(
                    "state-8 SIGTERM result was not preserved as a signal",
                ));
            }
        }
        _ => {
            let error = result.expect_err("state-8 parent accepted an adversarial completion");
            let (expected_kind, expected_message) = case
                .expected_error()
                .expect("non-success State-8 case has an expected error");
            if error.kind() != expected_kind || !error.to_string().contains(expected_message) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "state-8 parent returned {error:?}, expected {expected_kind:?} containing {expected_message:?}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn terminate_and_reap_raw_child(pid: libc::pid_t) {
    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid
            || (waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        {
            return;
        }
        if waited < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

fn run_state_8_protocol_peer(case: State8PeerCase, peer: &mut ControlChannel) -> io::Result<()> {
    if matches!(case, State8PeerCase::EofBeforeTargetExited) {
        return Ok(());
    }

    let original = CompletionAttestation {
        report: TargetExitedReport {
            raw_status: if matches!(case, State8PeerCase::SuccessSignal) {
                libc::SIGTERM
            } else {
                143 << 8
            },
            descendants_reaped: 0,
        },
        challenge: [case.tag().wrapping_add(0x40); COMPLETION_CHALLENGE_LEN],
    };
    match case {
        State8PeerCase::MalformedTargetExited => {
            return peer.send(
                FrameKind::TargetExited,
                vec![0; COMPLETION_ATTESTATION_LEN - 1],
            );
        }
        State8PeerCase::UnexpectedTargetFrame => {
            return peer.send(FrameKind::CleanupComplete, original.encode()?);
        }
        State8PeerCase::WrongSessionTargetExited => {
            let packet = Frame {
                session: [0xff; 32],
                sequence: 0,
                kind: FrameKind::TargetExited,
                payload: original.encode()?,
            }
            .encode()?;
            return send_raw_control_packet(peer.fd.as_raw_fd(), &packet);
        }
        State8PeerCase::ReplayedTargetExited => {
            let packet = Frame {
                session: peer.session,
                sequence: 0,
                kind: FrameKind::TargetExited,
                payload: original.encode()?,
            }
            .encode()?;
            send_raw_control_packet(peer.fd.as_raw_fd(), &packet)?;
            send_raw_control_packet(peer.fd.as_raw_fd(), &packet)?;
            thread::sleep(Duration::from_millis(300));
            return Ok(());
        }
        _ => peer.send(FrameKind::TargetExited, original.encode()?)?,
    }

    let request = peer.receive()?;
    if request.kind != FrameKind::CompleteSession {
        return Err(invalid_data("state-8 parent did not send CompleteSession"));
    }
    original.require_exact_echo(&request.payload)?;
    match peer.receive() {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
        Err(error) => return Err(error),
        Ok(frame) => {
            return Err(invalid_data(format!(
                "state-8 parent sent {:?} after CompleteSession",
                frame.kind
            )));
        }
    }

    match case {
        State8PeerCase::WrongCleanupReport => {
            let mut wrong = original;
            wrong.report.descendants_reaped = 1;
            peer.send(FrameKind::CleanupComplete, wrong.encode()?)
        }
        State8PeerCase::WrongCleanupCapability => {
            let mut wrong = original;
            wrong.challenge[0] ^= 0xff;
            peer.send(FrameKind::CleanupComplete, wrong.encode()?)
        }
        State8PeerCase::UnexpectedCleanupFrame => {
            peer.send(FrameKind::Fatal, b"synthetic peer fault".to_vec())
        }
        State8PeerCase::ReplayedCleanup => {
            let replay = Frame {
                session: peer.session,
                sequence: 0,
                kind: FrameKind::TargetExited,
                payload: original.encode()?,
            }
            .encode()?;
            send_raw_control_packet(peer.fd.as_raw_fd(), &replay)
        }
        State8PeerCase::ExtraAfterCleanup => {
            peer.send(FrameKind::CleanupComplete, original.encode()?)?;
            peer.send(FrameKind::Fatal, b"unexpected extra frame".to_vec())
        }
        State8PeerCase::EofBeforeCleanup | State8PeerCase::CancellationAfterRequest => Ok(()),
        State8PeerCase::MissingCleanupDeadline => {
            thread::sleep(DESCRIBE_TIMEOUT + Duration::from_millis(250));
            Ok(())
        }
        State8PeerCase::LateCleanup => {
            thread::sleep(Duration::from_millis(300));
            let _ = peer.send(FrameKind::CleanupComplete, original.encode()?);
            Ok(())
        }
        State8PeerCase::SuccessLiteral143
        | State8PeerCase::SuccessSignal
        | State8PeerCase::CancellationBeforeCommit
        | State8PeerCase::LateOuterExit
        | State8PeerCase::OuterNonzero
        | State8PeerCase::OuterSignaled => {
            peer.send(FrameKind::CleanupComplete, original.encode()?)?;
            if unsafe { libc::shutdown(peer.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        State8PeerCase::MalformedTargetExited
        | State8PeerCase::UnexpectedTargetFrame
        | State8PeerCase::ReplayedTargetExited
        | State8PeerCase::WrongSessionTargetExited
        | State8PeerCase::EofBeforeTargetExited => unreachable!(),
    }
}

fn wait_for_raw_child_status(pid: libc::pid_t, timeout: Duration) -> io::Result<libc::c_int> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return Ok(status);
        }
        if waited < 0 {
            return Err(io::Error::last_os_error());
        }
        if Instant::now() >= deadline {
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited != pid {
                return Err(io::Error::last_os_error());
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for state-8 protocol peer",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[allow(clippy::too_many_arguments)]
fn exercise_monitor_parent_session_case(
    case: State5HarnessCase,
    control: &mut ControlChannel,
    signal_writer: &mut Option<File>,
    child: &mut Child,
) -> io::Result<()> {
    let mut signal_sequence = 0u64;
    let expected_exit = match case {
        State5HarnessCase::SessionDefaultTerm | State5HarnessCase::SessionDescendants => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGTERM)?;
            ExpectedTargetExit::Signaled(libc::SIGTERM)
        }
        State5HarnessCase::SessionLiteralExit143
        | State5HarnessCase::SessionCancellation
        | State5HarnessCase::SessionOuterStatusMismatch => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            ExpectedTargetExit::Exited(143)
        }
        _ => unreachable!("non-state-8 case passed to parent session harness"),
    };

    let action = if matches!(case, State5HarnessCase::SessionCancellation) {
        ParentCompletionAction::CancelBeforeRequest
    } else {
        ParentCompletionAction::Complete
    };
    let completion = drive_parent_completion_session(control, child, action, DESCRIBE_TIMEOUT);

    match case {
        State5HarnessCase::SessionCancellation => {
            let error =
                completion.expect_err("state-8 cancellation unexpectedly returned a result");
            if error.kind() != io::ErrorKind::Interrupted
                || !error.to_string().contains("completion cancelled")
            {
                return Err(io::Error::new(
                    error.kind(),
                    format!("state-8 cancellation returned the wrong error: {error}"),
                ));
            }
            return Ok(());
        }
        State5HarnessCase::SessionOuterStatusMismatch => {
            let error = completion.expect_err("state-8 accepted a nonzero outer status");
            if !error.to_string().contains("outer process exited") {
                return Err(io::Error::new(
                    error.kind(),
                    format!("state-8 outer mismatch returned the wrong error: {error}"),
                ));
            }
            return Ok(());
        }
        _ => {}
    }

    let completion = completion?;
    expected_exit.verify(completion.report.raw_status)?;
    let expected_descendants = u32::from(matches!(case, State5HarnessCase::SessionDescendants)) * 2;
    if completion.report.descendants_reaped != expected_descendants {
        return Err(invalid_data(format!(
            "state-8 completion reported {} descendant reaps, expected {expected_descendants}",
            completion.report.descendants_reaped
        )));
    }
    if completion.status.into_raw() != completion.report.raw_status {
        return Err(invalid_data(
            "state-8 child result was not derived from the original raw target status",
        ));
    }
    Ok(())
}

fn drive_parent_completion_session(
    control: &mut ControlChannel,
    outer: &mut Child,
    action: ParentCompletionAction,
    completion_timeout: Duration,
) -> io::Result<ParentCompletionResult> {
    let mut cancellation_checks = 0usize;
    let mut cancellation = || {
        let check = cancellation_checks;
        cancellation_checks = cancellation_checks.saturating_add(1);
        match action {
            ParentCompletionAction::Complete => false,
            ParentCompletionAction::CancelBeforeRequest => check == 0,
            ParentCompletionAction::CancelAfterRequest => check == 1,
            ParentCompletionAction::CancelBeforeCommit => check == 5,
        }
    };
    let outcome = drive_parent_completion_session_inner(
        control,
        outer,
        &mut cancellation,
        completion_timeout,
    );
    match outcome {
        Ok(result) => Ok(result),
        Err(error) => match fail_closed_outer(outer) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "{error}; state-8 fail-closed cleanup failed: {cleanup}"
            ))),
        },
    }
}

fn drive_parent_completion_session_inner(
    control: &mut ControlChannel,
    outer: &mut Child,
    cancellation: &mut impl FnMut() -> bool,
    completion_timeout: Duration,
) -> io::Result<ParentCompletionResult> {
    // Cleanup ownership is armed by the outer wrapper before this first read.
    let first = receive_control_until(control, Instant::now() + DESCRIBE_TIMEOUT)?;
    if first.kind != FrameKind::TargetExited {
        return Err(invalid_data(format!(
            "state-8 expected TargetExited, received {:?}",
            first.kind
        )));
    }
    let original = CompletionAttestation::decode(&first.payload)?;
    let deadline = Instant::now() + completion_timeout;
    reject_parent_completion_cancellation(cancellation)?;

    control.send_with_deadline(
        FrameKind::CompleteSession,
        original.encode()?,
        Some(deadline),
    )?;
    shutdown_control_write(control)?;
    reject_parent_completion_cancellation(cancellation)?;

    let cleanup = receive_control_until(control, deadline)?;
    if cleanup.kind != FrameKind::CleanupComplete {
        return Err(invalid_data(format!(
            "state-8 expected CleanupComplete, received {:?}",
            cleanup.kind
        )));
    }
    original.require_exact_echo(&cleanup.payload)?;
    reject_parent_completion_cancellation(cancellation)?;
    match receive_control_until(control, deadline) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
        Err(error) => return Err(error),
        Ok(frame) => {
            return Err(invalid_data(format!(
                "state-8 received unexpected post-completion {:?} frame",
                frame.kind
            )));
        }
    }
    reject_parent_completion_cancellation(cancellation)?;

    let outer_status = wait_for_child_status_until(outer, deadline)?;
    if outer_status.code() != Some(0) {
        return Err(invalid_data(format!(
            "state-8 outer process exited with {outer_status}, expected code 0"
        )));
    }
    reject_parent_completion_cancellation(cancellation)?;
    ensure_before_deadline(deadline, "state-8 completion deadline elapsed")?;
    // This observation is the result-commit linearization point. Cancellation
    // observed before it wins; after the result is returned it is irrelevant.
    reject_parent_completion_cancellation(cancellation)?;
    ensure_before_deadline(deadline, "state-8 completion deadline elapsed")?;

    // Result commitment is last. Neither CleanupComplete nor the normalized
    // outer process status can replace the immutable original target status.
    Ok(ParentCompletionResult {
        status: ExitStatus::from_raw(original.report.raw_status),
        report: original.report,
    })
}

fn reject_parent_completion_cancellation(
    cancellation: &mut impl FnMut() -> bool,
) -> io::Result<()> {
    if cancellation() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "state-8 completion cancelled before result commitment",
        ))
    } else {
        Ok(())
    }
}

fn exercise_monitor_completion_case(
    case: State5HarnessCase,
    control: &mut ControlChannel,
    signal_writer: &mut Option<File>,
    child: &mut std::process::Child,
    target_pid: libc::pid_t,
    attestation: TargetStoppedAttestation,
) -> io::Result<()> {
    if matches!(case, State5HarnessCase::CompletionEarlyRequest) {
        let early = CompletionAttestation {
            report: TargetExitedReport {
                raw_status: 143 << 8,
                descendants_reaped: 0,
            },
            challenge: [0x4d; COMPLETION_CHALLENGE_LEN],
        };
        control.send(FrameKind::CompleteSession, early.encode()?)?;
        expect_fatal_contains(control, "running target accepts only")?;
        require_outer_status(child, Some(125))?;
        return Ok(());
    }

    let mut signal_sequence = 0u64;
    let expected_exit = match case {
        State5HarnessCase::CompletionDefaultTerm | State5HarnessCase::CompletionDescendants => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGTERM)?;
            ExpectedTargetExit::Signaled(libc::SIGTERM)
        }
        completion if completion.is_state_7() => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGUSR1)?;
            ExpectedTargetExit::Exited(143)
        }
        _ => unreachable!("non-state-7 case passed to completion harness"),
    };

    let completion = expect_completion_attestation(control)?;
    expected_exit.verify(completion.report.raw_status)?;
    let expected_descendants =
        u32::from(matches!(case, State5HarnessCase::CompletionDescendants)) * 2;
    if completion.report.descendants_reaped != expected_descendants {
        return Err(invalid_data(format!(
            "completion attestation reported {} descendant reaps, expected {expected_descendants}",
            completion.report.descendants_reaped
        )));
    }

    match case {
        State5HarnessCase::CompletionLiteralExit143
        | State5HarnessCase::CompletionDefaultTerm
        | State5HarnessCase::CompletionDescendants => {
            control.send(FrameKind::CompleteSession, completion.encode()?)?;
            shutdown_control_write(control)?;
            let cleanup = expect_cleanup_complete(control)?;
            completion.require_exact_echo(&cleanup.encode()?)?;
            expect_control_eof(control)?;
            require_outer_status(child, Some(0))?;
        }
        State5HarnessCase::CompletionWrongAttestation => {
            let mut wrong = completion;
            wrong.challenge[0] ^= 0xff;
            control.send(FrameKind::CompleteSession, wrong.encode()?)?;
            shutdown_control_write(control)?;
            expect_fatal_contains(control, "exact attestation")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionMalformed => {
            control.send(
                FrameKind::CompleteSession,
                vec![0; COMPLETION_ATTESTATION_LEN - 1],
            )?;
            shutdown_control_write(control)?;
            expect_fatal_contains(control, "attestation length")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionUnexpected => {
            control.send(FrameKind::Terminate, Vec::new())?;
            shutdown_control_write(control)?;
            expect_fatal_contains(control, "unexpected Terminate")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionDuplicate => {
            let payload = completion.encode()?;
            control.send(FrameKind::CompleteSession, payload.clone())?;
            control.send(FrameKind::CompleteSession, payload)?;
            shutdown_control_write(control)?;
            expect_fatal_contains(control, "additional CompleteSession")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionReplay => {
            let request_sequence = control.send_sequence;
            let payload = completion.encode()?;
            control.send(FrameKind::CompleteSession, payload.clone())?;
            let replay = Frame {
                session: control.session,
                sequence: request_sequence,
                kind: FrameKind::CompleteSession,
                payload,
            }
            .encode()?;
            send_raw_control_packet(control.fd.as_raw_fd(), &replay)?;
            shutdown_control_write(control)?;
            expect_fatal_contains(control, "sequence mismatch or replay")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionEofBeforeRequest => {
            shutdown_control_write(control)?;
            expect_fatal_contains(control, "closed before its request")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionRelayInput => {
            send_signal_relay_record(signal_writer, &mut signal_sequence, libc::SIGCONT)?;
            // Once TargetExited is authenticated the parent disarms its relay.
            // A record already queued before that disarm is obsolete input, not a
            // new signal authority, and the monitor must drain it without turning a
            // valid completion into a protocol failure.
            control.send(FrameKind::CompleteSession, completion.encode()?)?;
            shutdown_control_write(control)?;
            let cleanup = expect_cleanup_complete(control)?;
            completion.require_exact_echo(&cleanup.encode()?)?;
            expect_control_eof(control)?;
            require_outer_status(child, Some(0))?;
        }
        State5HarnessCase::CompletionRequestMayWait => {
            // TargetExited can precede the caller's eventual wait() by an arbitrary
            // amount of time. No completion deadline exists until CompleteSession
            // is received; prove the monitor neither exits nor invents a packet at
            // the former handshake timeout, then finish the exact protocol.
            thread::sleep(DESCRIBE_TIMEOUT + Duration::from_millis(250));
            if child.try_wait()?.is_some() || control_packet_available(control.fd.as_raw_fd())? {
                return Err(invalid_data(
                    "completion monitor timed out before the parent's request",
                ));
            }
            control.send(FrameKind::CompleteSession, completion.encode()?)?;
            shutdown_control_write(control)?;
            let cleanup = expect_cleanup_complete(control)?;
            completion.require_exact_echo(&cleanup.encode()?)?;
            expect_control_eof(control)?;
            require_outer_status(child, Some(0))?;
        }
        State5HarnessCase::CompletionDeadlineAfterRequest => {
            set_control_timeouts(control.fd.as_raw_fd(), DESCRIBE_TIMEOUT.saturating_mul(2))?;
            control.send(FrameKind::CompleteSession, completion.encode()?)?;
            shutdown_control_write(control)?;
            expect_fatal_or_closed(control, "sandbox completion deadline elapsed")?;
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionSendFailure => {
            control.send(FrameKind::CompleteSession, completion.encode()?)?;
            shutdown_control_write(control)?;
            if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_RD) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // CleanupComplete and the best-effort Fatal are both intentionally
            // undeliverable. The outer status and empty process tree are the oracle.
            require_outer_status(child, Some(125))?;
        }
        State5HarnessCase::CompletionEarlyRequest => unreachable!(),
        _ => unreachable!("non-state-7 case passed to completion harness"),
    }

    wait_for_host_target_exit(target_pid, attestation.starttime, DESCRIBE_TIMEOUT)?;
    Ok(())
}

fn expect_completion_attestation(
    control: &mut ControlChannel,
) -> io::Result<CompletionAttestation> {
    let frame = control.receive()?;
    if frame.kind != FrameKind::TargetExited {
        return Err(invalid_data(
            "monitor did not publish an authenticated completion attestation",
        ));
    }
    CompletionAttestation::decode(&frame.payload)
}

fn expect_cleanup_complete(control: &mut ControlChannel) -> io::Result<CompletionAttestation> {
    let frame = control.receive()?;
    if frame.kind != FrameKind::CleanupComplete {
        return Err(invalid_data(
            "monitor did not publish authenticated cleanup completion",
        ));
    }
    CompletionAttestation::decode(&frame.payload)
}

fn shutdown_control_write(control: &ControlChannel) -> io::Result<()> {
    if unsafe { libc::shutdown(control.fd.as_raw_fd(), libc::SHUT_WR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn expect_control_eof(control: &mut ControlChannel) -> io::Result<()> {
    match control.receive_with_deadline(Some(Instant::now() + DESCRIBE_TIMEOUT)) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(error) => Err(error),
        Ok(frame) => Err(invalid_data(format!(
            "sandbox completion received unexpected post-completion {:?} frame",
            frame.kind
        ))),
    }
}

fn expect_fatal_or_closed(control: &mut ControlChannel, needle: &str) -> io::Result<()> {
    match control.receive_with_deadline(Some(Instant::now() + DESCRIBE_TIMEOUT.saturating_mul(2))) {
        Ok(frame) if frame.kind == FrameKind::Fatal => {
            let message = String::from_utf8_lossy(&frame.payload);
            if message.contains(needle) {
                Ok(())
            } else {
                Err(invalid_data(format!(
                    "monitor fatal did not contain {needle:?}: {message}"
                )))
            }
        }
        Ok(frame) => Err(invalid_data(format!(
            "sandbox completion unexpectedly published {:?}",
            frame.kind
        ))),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn require_outer_status(
    child: &mut std::process::Child,
    expected_code: Option<i32>,
) -> io::Result<()> {
    let status = wait_for_child_status(child, DESCRIBE_TIMEOUT)?;
    if status.code() != expected_code {
        return Err(invalid_data(format!(
            "sandbox monitor outer process exited with {status}, expected code {expected_code:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ExpectedTargetExit {
    Exited(libc::c_int),
    Signaled(libc::c_int),
    ExitedOrKilled(libc::c_int, libc::c_int),
}

impl ExpectedTargetExit {
    fn verify(self, raw_status: libc::c_int) -> io::Result<()> {
        let matches = match self {
            Self::Exited(code) => {
                libc::WIFEXITED(raw_status) && libc::WEXITSTATUS(raw_status) == code
            }
            Self::Signaled(signal) => {
                libc::WIFSIGNALED(raw_status) && libc::WTERMSIG(raw_status) == signal
            }
            Self::ExitedOrKilled(code, signal) => {
                (libc::WIFEXITED(raw_status) && libc::WEXITSTATUS(raw_status) == code)
                    || (libc::WIFSIGNALED(raw_status) && libc::WTERMSIG(raw_status) == signal)
            }
        };
        if matches {
            Ok(())
        } else {
            Err(invalid_data(format!(
                "monitor reported unexpected raw target wait status {raw_status:#x}"
            )))
        }
    }
}

fn expect_target_exited(control: &mut ControlChannel) -> io::Result<TargetExitedReport> {
    let frame = control.receive()?;
    if frame.kind != FrameKind::TargetExited {
        return Err(invalid_data(
            "monitor did not publish an authenticated target-exited report",
        ));
    }
    TargetExitedReport::decode(&frame.payload)
}

fn send_signal_relay_record(
    writer: &mut Option<File>,
    sequence: &mut u64,
    signal: libc::c_int,
) -> io::Result<()> {
    let record = SignalRelayRecord {
        sequence: *sequence,
        signal,
    }
    .encode()?;
    write_signal_relay_packet(writer, &record)?;
    *sequence = sequence
        .checked_add(1)
        .ok_or_else(|| invalid_data("test signal relay sequence overflow"))?;
    Ok(())
}

fn write_signal_relay_packet(writer: &mut Option<File>, packet: &[u8]) -> io::Result<()> {
    let writer = writer
        .as_mut()
        .ok_or_else(|| invalid_input("test signal relay writer is closed"))?;
    let deadline = Instant::now() + DESCRIBE_TIMEOUT;
    loop {
        ensure_before_deadline(deadline, "test signal relay write deadline elapsed")?;
        let written =
            unsafe { libc::write(writer.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
        ensure_before_deadline(deadline, "test signal relay write deadline elapsed")?;
        if written >= 0 {
            if written as usize == packet.len() {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short test signal relay packet",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        let mut pollfd = libc::pollfd {
            fd: writer.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining.as_millis().clamp(1, 10) as libc::c_int;
        let polled = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

fn send_raw_control_packet(fd: RawFd, packet: &[u8]) -> io::Result<()> {
    let written =
        unsafe { libc::send(fd, packet.as_ptr().cast(), packet.len(), libc::MSG_NOSIGNAL) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    if written as usize != packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short raw sandbox control packet",
        ));
    }
    Ok(())
}

fn wait_for_runtime_ready_name(pid: libc::pid_t) -> io::Result<()> {
    let deadline = Instant::now() + DESCRIBE_TIMEOUT;
    loop {
        match fs::read_to_string(format!("/proc/{pid}/comm")) {
            Ok(name) if name.trim_end() == "nub-s6-ready" => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(error),
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for the state-6 target readiness marker",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn open_descriptor_pressure_through(minimum_last_fd: RawFd) -> io::Result<Vec<File>> {
    let mut files = Vec::new();
    while files
        .last()
        .is_none_or(|file: &File| file.as_raw_fd() < minimum_last_fd)
    {
        if files.len() >= 1024 {
            return Err(invalid_data(
                "descriptor-pressure harness exceeded its file budget",
            ));
        }
        files.push(File::open("/dev/null")?);
    }
    Ok(files)
}

fn target_probe_command(image: &RuntimeImage, verb: &str) -> (OsString, Vec<OsString>) {
    let root = Path::new(PRIVATE_RUNTIME_ROOT);
    match &image.kind {
        RuntimeKind::Static => (
            root.join("nub-monitor").into_os_string(),
            vec![OsString::from(verb)],
        ),
        RuntimeKind::Dynamic {
            family,
            inhibit_rpath,
            ..
        } => {
            let mut args = Vec::new();
            if *family == LoaderFamily::Glibc {
                args.push(OsString::from("--inhibit-cache"));
            }
            args.push(OsString::from("--library-path"));
            args.push(root.join("lib").into_os_string());
            if *family == LoaderFamily::Glibc {
                args.push(OsString::from("--inhibit-rpath"));
                args.push(inhibit_rpath.clone());
            }
            args.push(root.join("nub-monitor").into_os_string());
            args.push(OsString::from(verb));
            (root.join("ld.so").into_os_string(), args)
        }
    }
}

fn verify_accepted_target(
    pid: libc::pid_t,
    monitor_pid: libc::pid_t,
    attestation: TargetStoppedAttestation,
    uses_seccomp: bool,
    expected_program: &OsStr,
    expected_args: &[OsString],
) -> io::Result<()> {
    wait_for_host_process_state(pid, b'S', DESCRIBE_TIMEOUT)?;
    verify_target_host_state(
        pid,
        monitor_pid,
        attestation,
        ExpectedTargetState::Live,
        uses_seccomp,
    )?;
    if fs::read_link(format!("/proc/{pid}/cwd"))? != Path::new("/") {
        return Err(invalid_data(
            "accepted target did not retain its requested cwd",
        ));
    }
    if fs::read(format!("/proc/{pid}/environ"))? != b"TARGET_EXEC_PROBE=state5\0" {
        return Err(invalid_data(
            "accepted target did not retain its exact requested environment",
        ));
    }
    let command_line = fs::read(format!("/proc/{pid}/cmdline"))?;
    let arguments = command_line
        .split(|byte| *byte == 0)
        .take_while(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    let expected_arguments = std::iter::once(expected_program.as_bytes())
        .chain(expected_args.iter().map(|argument| argument.as_bytes()))
        .collect::<Vec<_>>();
    if arguments != expected_arguments {
        return Err(invalid_data(
            "accepted target did not retain its requested argument vector",
        ));
    }
    Ok(())
}

fn expect_fatal_contains(control: &mut ControlChannel, needle: &str) -> io::Result<()> {
    expect_fatal_contains_any(control, &[needle])
}

fn expect_fatal_contains_any(control: &mut ControlChannel, needles: &[&str]) -> io::Result<()> {
    let fatal = control.receive()?;
    let payload = String::from_utf8_lossy(&fatal.payload);
    if fatal.kind != FrameKind::Fatal || !needles.iter().any(|needle| payload.contains(needle)) {
        return Err(invalid_data(
            "monitor did not send the expected fatal result",
        ));
    }
    Ok(())
}

fn runtime_bindings(image: &RuntimeImage) -> io::Result<Vec<File>> {
    let mut files = vec![duplicate_above_stdio(&image.executable.file)?];
    if let RuntimeKind::Dynamic {
        loader, libraries, ..
    } = &image.kind
    {
        files.push(duplicate_above_stdio(&loader.file)?);
        for library in libraries {
            files.push(duplicate_above_stdio(&library.file)?);
        }
    }
    Ok(files)
}

fn append_monitor_command(command: &mut std::process::Command, image: &RuntimeImage) {
    let root = Path::new(PRIVATE_RUNTIME_ROOT);
    match &image.kind {
        RuntimeKind::Static => {
            command.arg(root.join("nub-monitor"));
        }
        RuntimeKind::Dynamic {
            family,
            inhibit_rpath,
            ..
        } => {
            command.arg(root.join("ld.so"));
            if *family == LoaderFamily::Glibc {
                command.arg("--inhibit-cache");
            }
            command.arg("--library-path").arg(root.join("lib"));
            if *family == LoaderFamily::Glibc {
                command.arg("--inhibit-rpath").arg(inhibit_rpath);
            }
            command.arg(root.join("nub-monitor"));
        }
    }
}

fn sealed_memfd(bytes: &[u8]) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"nub-sandbox-monitor-bootstrap".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as RawFd
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = File::from(fd.try_clone()?);
    file.write_all(bytes)?;
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_BOOTSTRAP_SEALS) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn pipe_files(nonblocking: bool) -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    let flags = libc::O_CLOEXEC | if nonblocking { libc::O_NONBLOCK } else { 0 };
    if unsafe { libc::pipe2(fds.as_mut_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

fn signal_relay_pipe_files() -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    if unsafe {
        libc::pipe2(
            fds.as_mut_ptr(),
            libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_DIRECT,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let reader_flags = unsafe { libc::fcntl(fds[0], libc::F_GETFL) };
    if reader_flags < 0
        || unsafe { libc::fcntl(fds[0], libc::F_SETFL, reader_flags | libc::O_DIRECT) } != 0
    {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(error);
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn mark_inherited_fds_cloexec() -> io::Result<()> {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    let result =
        unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) };
    if result >= 0 {
        return Ok(());
    }
    unsafe { mark_open_fds_cloexec_from_proc() }
}

unsafe fn mark_open_fds_cloexec_from_proc() -> io::Result<()> {
    const PROC_SUPER_MAGIC: libc::c_long = 0x9fa0;
    const DIRENT_HEADER: usize = 19;
    let directory = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        ) as RawFd
    };
    if directory < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory, stat.as_mut_ptr()) } != 0
        || unsafe { stat.assume_init() }.f_type != PROC_SUPER_MAGIC
    {
        unsafe { libc::close(directory) };
        return Err(io::Error::other(
            "/proc/self/fd is not a procfs descriptor directory",
        ));
    }
    let mut buffer = [0u8; 8192];
    let mut saw_directory = false;
    loop {
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
            ) as isize
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe { libc::close(directory) };
            return Err(error);
        }
        if count == 0 {
            break;
        }
        let count = count as usize;
        let mut offset = 0usize;
        while offset < count {
            if count - offset < DIRENT_HEADER {
                unsafe { libc::close(directory) };
                return Err(io::Error::other(
                    "procfs descriptor enumeration returned a truncated record",
                ));
            }
            let record = &buffer[offset..count];
            let reclen = u16::from_ne_bytes([record[16], record[17]]) as usize;
            if reclen < DIRENT_HEADER || offset + reclen > count {
                unsafe { libc::close(directory) };
                return Err(io::Error::other(
                    "procfs descriptor enumeration returned a malformed record",
                ));
            }
            let name = &record[DIRENT_HEADER..reclen];
            let Some(end) = name.iter().position(|byte| *byte == 0) else {
                unsafe { libc::close(directory) };
                return Err(io::Error::other(
                    "procfs descriptor enumeration returned an unterminated name",
                ));
            };
            let name = &name[..end];
            if name != b"." && name != b".." {
                if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
                    unsafe { libc::close(directory) };
                    return Err(io::Error::other(
                        "procfs descriptor enumeration returned a nonnumeric name",
                    ));
                }
                let mut fd: RawFd = 0;
                for byte in name {
                    fd = fd
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(i32::from(*byte - b'0')))
                        .ok_or_else(|| {
                            io::Error::other(
                                "procfs descriptor enumeration overflowed a descriptor number",
                            )
                        })?;
                }
                if fd == directory {
                    saw_directory = true;
                } else if fd >= 3 {
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    if flags < 0
                        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
                    {
                        let error = io::Error::last_os_error();
                        unsafe { libc::close(directory) };
                        return Err(error);
                    }
                }
            }
            offset += reclen;
        }
    }
    unsafe { libc::close(directory) };
    if !saw_directory {
        return Err(io::Error::other(
            "procfs descriptor enumeration omitted its own descriptor",
        ));
    }
    Ok(())
}

fn read_bwrap_child_pid(
    reader: &mut File,
    child: &mut std::process::Child,
) -> io::Result<libc::pid_t> {
    read_bwrap_child_pid_until(reader, child, Instant::now() + DESCRIBE_TIMEOUT)
}

fn read_bwrap_child_pid_until(
    reader: &mut File,
    child: &mut std::process::Child,
    deadline: Instant,
) -> io::Result<libc::pid_t> {
    #[derive(serde::Deserialize)]
    struct Info {
        #[serde(rename = "child-pid")]
        child_pid: libc::pid_t,
    }
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 512];
        match reader.read(&mut chunk) {
            Ok(0) => {}
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > MAX_FRAME_PAYLOAD {
                    return Err(invalid_data("Bubblewrap info record exceeded its budget"));
                }
                if let Ok(info) = serde_json::from_slice::<Info>(&bytes) {
                    return Ok(info.child_pid);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "Bubblewrap exited before monitor identity: {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for Bubblewrap monitor identity",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn verify_monitor_host_state(pid: libc::pid_t, bwrap_pid: u32) -> io::Result<()> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
    };
    let bwrap_pid = bwrap_pid.to_string();
    if field("PPid:") != Some(bwrap_pid.as_str())
        || !field("NSpid:").is_some_and(|value| value.ends_with("\t1") || value == "1")
        || !valid_no_new_privs_status(field("NoNewPrivs:"))
    {
        return Err(invalid_data("monitor host process identity is invalid"));
    }
    for capability in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        if field(capability) != Some("0000000000000000") {
            return Err(invalid_data("monitor host process retained capabilities"));
        }
    }
    match fs::read(format!("/proc/{pid}/environ")) {
        // `/proc/<pid>/environ` exposes the exec-time environment memory, not
        // libc's later `clearenv()` state. Stock Bubblewrap adds only this PWD
        // value after `--clearenv`; the authenticated monitor clears it and
        // verifies the live libc environment before emitting MonitorReady.
        Ok(environment) if valid_monitor_exec_environment(&environment) => {}
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // PR_SET_DUMPABLE=0 commonly makes this unreadable even to the
            // same-UID ancestor. MonitorReady is emitted only after the monitor
            // itself has cleared and verified the environment.
        }
        Ok(environment) => {
            let entries = environment
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
                .count();
            let first = environment
                .split(|byte| *byte == 0)
                .find(|entry| !entry.is_empty())
                .unwrap_or_default();
            let key_bytes = first
                .iter()
                .position(|byte| *byte == b'=')
                .map_or(first, |equals| &first[..equals]);
            let pwd_prefix = environment.starts_with(b"PWD=/\0");
            let zero_suffix = pwd_prefix && environment[b"PWD=/\0".len()..].iter().all(|b| *b == 0);
            return Err(invalid_data(format!(
                "monitor host process environment is not empty (bytes={}, entries={entries}, key-bytes={}, pwd-key={}, path-key={}, glibc-key={}, ld-library-key={}, bwrap-key={}, nub-key={}, pwd-prefix={pwd_prefix}, zero-suffix={zero_suffix})",
                environment.len(),
                key_bytes.len(),
                key_bytes == b"PWD",
                key_bytes == b"PATH",
                key_bytes == b"GLIBC_TUNABLES",
                key_bytes == b"LD_LIBRARY_PATH",
                key_bytes.starts_with(b"BWRAP"),
                key_bytes.starts_with(b"NUB"),
            )));
        }
        Err(error) => return Err(error),
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| invalid_data("monitor host stat record is malformed"))?;
    let fields = stat[close + 2..].split_whitespace().collect::<Vec<_>>();
    let pid = pid.to_string();
    if fields.get(2).copied() != Some(pid.as_str()) || fields.get(3).copied() != Some(pid.as_str())
    {
        return Err(invalid_data("monitor is not its host session/group leader"));
    }
    Ok(())
}

fn valid_monitor_exec_environment(environment: &[u8]) -> bool {
    environment.is_empty() || environment == b"PWD=/\0"
}

fn read_unique_monitor_child(monitor_pid: libc::pid_t) -> io::Result<libc::pid_t> {
    let children = fs::read_to_string(format!("/proc/{monitor_pid}/task/{monitor_pid}/children"))?;
    let children = children
        .split_ascii_whitespace()
        .map(|value| {
            value
                .parse::<libc::pid_t>()
                .map_err(|_| invalid_data("monitor child list contains an invalid PID"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    match children.as_slice() {
        [pid] if *pid > 0 => Ok(*pid),
        _ => Err(invalid_data(
            "monitor does not have exactly one stopped target child",
        )),
    }
}

fn duplicate_target_exec_error_writer(pid: libc::pid_t) -> io::Result<File> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
        let fd = entry?
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("target descriptor name is not numeric"))?
            .parse::<RawFd>()
            .map_err(|_| invalid_data("target descriptor name is not numeric"))?;
        if fd <= libc::STDERR_FILENO {
            continue;
        }
        let info = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))?;
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:").map(str::trim))
            .ok_or_else(|| invalid_data("target descriptor info omitted flags"))?;
        let flags = libc::c_int::from_str_radix(flags, 8)
            .map_err(|_| invalid_data("target descriptor flags are invalid"))?;
        if flags & libc::O_ACCMODE == libc::O_WRONLY {
            candidates.push(fd);
        }
    }
    let [fd] = candidates.as_slice() else {
        return Err(invalid_data(
            "target does not have exactly one exec-error writer",
        ));
    };
    fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{pid}/fd/{fd}"))
}

#[derive(Debug, Clone, Copy)]
enum ExpectedTargetState {
    Stopped,
    Live,
}

fn verify_target_host_state(
    pid: libc::pid_t,
    monitor_pid: libc::pid_t,
    attestation: TargetStoppedAttestation,
    expected_state: ExpectedTargetState,
    uses_seccomp: bool,
) -> io::Result<()> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
    };
    let monitor_text = monitor_pid.to_string();
    let namespace_text = attestation.namespace_pid.to_string();
    if field("PPid:") != Some(monitor_text.as_str())
        || !field("NSpid:").is_some_and(|value| {
            value
                .split_ascii_whitespace()
                .next_back()
                .is_some_and(|value| value == namespace_text.as_str())
        })
        || !valid_no_new_privs_status(field("NoNewPrivs:"))
    {
        return Err(invalid_data("target host process identity is invalid"));
    }
    for capability in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        if field(capability) != Some("0000000000000000") {
            return Err(invalid_data("target host process retained capabilities"));
        }
    }
    if uses_seccomp && field("Seccomp:") != Some("2") {
        return Err(invalid_data(
            "target host process omitted its required seccomp filter",
        ));
    }

    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| invalid_data("target host stat record is malformed"))?;
    let fields = stat[close + 2..].split_whitespace().collect::<Vec<_>>();
    let pid_text = pid.to_string();
    let state_valid = match (expected_state, fields.first().copied()) {
        (ExpectedTargetState::Stopped, Some("T" | "t")) => true,
        (ExpectedTargetState::Live, Some(state)) => !matches!(state, "T" | "t" | "Z" | "X"),
        _ => false,
    };
    if !state_valid
        || fields.get(1).copied() != Some(monitor_text.as_str())
        || fields.get(2).copied() != Some(pid_text.as_str())
        || fields.get(3).copied() != Some(monitor_text.as_str())
    {
        return Err(invalid_data(format!(
            "target host process shape is invalid: observed {:?}, expected state={expected_state:?} ppid={monitor_text} pgrp={pid_text} session={monitor_text}",
            &fields[..fields.len().min(4)]
        )));
    }
    let starttime = fields
        .get(19)
        .ok_or_else(|| invalid_data("target host stat omits starttime"))?
        .parse::<u64>()
        .map_err(|_| invalid_data("target host starttime is invalid"))?;
    if starttime != attestation.starttime {
        return Err(invalid_data("target host starttime attestation mismatch"));
    }
    Ok(())
}

fn wait_for_host_target_exit(
    pid: libc::pid_t,
    starttime: u64,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(error) if process_disappeared(&error) => return Ok(()),
            Err(error) => return Err(error),
            Ok(stat) => {
                let close = stat
                    .rfind(')')
                    .ok_or_else(|| invalid_data("target host stat record is malformed"))?;
                let fields = stat[close + 2..].split_whitespace().collect::<Vec<_>>();
                let observed = fields
                    .get(19)
                    .ok_or_else(|| invalid_data("target host stat omits starttime"))?
                    .parse::<u64>()
                    .map_err(|_| invalid_data("target host starttime is invalid"))?;
                if observed != starttime {
                    return Ok(());
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for stopped target cleanup",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn process_disappeared(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn valid_no_new_privs_status(value: Option<&str>) -> bool {
    matches!(value, None | Some("1"))
}

fn wait_for_host_process_state(pid: libc::pid_t, state: u8, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = fs::read(format!("/proc/{pid}/status"))?;
        if proc_status_state(&status) == Some(state) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out observing monitor stop",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_host_process_not_state(
    pid: libc::pid_t,
    state: u8,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = fs::read(format!("/proc/{pid}/status"))?;
        if proc_status_state(&status) != Some(state) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out resuming monitor after hostile SIGCONT",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn proc_status_state(status: &[u8]) -> Option<u8> {
    status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"State:"))?
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn control_packet_available(fd: RawFd) -> io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result != 0)
}

fn signal_relay_packet_available(fd: RawFd) -> io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result != 0)
}

fn receive_signal_relay_record(
    fd: RawFd,
    expected_sequence: u64,
    deadline: Instant,
) -> io::Result<Option<SignalRelayRecord>> {
    let mut bytes = [0u8; SIGNAL_RELAY_RECORD_LEN + 1];
    let received = loop {
        ensure_before_deadline(deadline, "sandbox signal relay receive deadline elapsed")?;
        let received = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        ensure_before_deadline(deadline, "sandbox signal relay receive deadline elapsed")?;
        if received > 0 {
            break received as usize;
        }
        if received == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sandbox signal relay closed",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error);
    };
    SignalRelayRecord::decode(&bytes[..received], expected_sequence).map(Some)
}

fn ensure_before_deadline(deadline: Instant, message: &str) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(io::ErrorKind::TimedOut, message));
    }
    Ok(())
}

fn require_quiet_exec_control(control: &mut ControlChannel, deadline: Instant) -> io::Result<()> {
    if control_packet_available(control.fd.as_raw_fd())? {
        ensure_before_deadline(deadline, "timed out awaiting target exec acceptance")?;
        let frame = control.receive_with_deadline(Some(deadline))?;
        return Err(invalid_data(format!(
            "unexpected {:?} control frame during target exec",
            frame.kind
        )));
    }
    Ok(())
}

fn receive_control_until(control: &mut ControlChannel, deadline: Instant) -> io::Result<Frame> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sandbox monitor control deadline elapsed",
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout = remaining
            .as_nanos()
            .saturating_add(999_999)
            .div_euclid(1_000_000)
            .clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        let mut pollfd = libc::pollfd {
            fd: control.fd.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if result > 0 {
            ensure_before_deadline(deadline, "sandbox monitor control deadline elapsed")?;
            return control.receive_with_deadline(Some(deadline));
        }
        if result == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn set_control_timeouts(fd: RawFd, timeout: Duration) -> io::Result<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        if unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                (&value as *const libc::timeval).cast(),
                mem::size_of_val(&value) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn wait_for_child_exit(child: &mut std::process::Child, timeout: Duration) -> io::Result<()> {
    wait_for_child_status(child, timeout).map(|_| ())
}

fn wait_for_child_status(
    child: &mut std::process::Child,
    timeout: Duration,
) -> io::Result<std::process::ExitStatus> {
    wait_for_child_status_until(child, Instant::now() + timeout)
}

fn wait_for_child_status_until(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
    loop {
        ensure_before_deadline(deadline, "timed out waiting for held monitor probe to exit")?;
        if let Some(status) = child.try_wait()? {
            ensure_before_deadline(deadline, "timed out waiting for held monitor probe to exit")?;
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program name here is deliberately option-shaped: it is the case
    /// bubblewrap's own suite covers with `--`.
    #[test]
    fn command_position_is_closed_by_a_separator() {
        let mut command = Command::new("bwrap");
        append_command_position(
            &mut command,
            OsStr::new("--not-a-security-boundary"),
            &[OsString::from("--inhibit-cache")],
        );
        let argv: Vec<_> = command.get_args().collect();
        assert_eq!(
            argv,
            [
                OsStr::new("--"),
                OsStr::new("--not-a-security-boundary"),
                OsStr::new("--inhibit-cache"),
                OsStr::new(MONITOR_SENTINEL),
            ]
        );
    }

    fn owned_pair(kind: libc::c_int) -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    kind | libc::SOCK_CLOEXEC,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        }
    }

    fn memfd_with(contents: &[u8], sealed: bool) -> OwnedFd {
        let descriptor = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"nub-sandbox-bootstrap-test".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            ) as RawFd
        };
        assert!(descriptor >= 0, "{}", io::Error::last_os_error());
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let mut file = File::from(descriptor.try_clone().unwrap());
        file.write_all(contents).unwrap();
        if sealed {
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        descriptor.as_raw_fd(),
                        libc::F_ADD_SEALS,
                        REQUIRED_BOOTSTRAP_SEALS,
                    )
                },
                0
            );
        }
        descriptor
    }

    fn release_pipe(contents: &[u8]) -> OwnedFd {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let mut writer = File::from(writer);
        writer.write_all(contents).unwrap();
        drop(writer);
        reader
    }

    fn send_with_descriptor(socket: RawFd, bytes: &[u8], descriptor: RawFd) {
        let mut iovec = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let control_len = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
        let mut ancillary = aligned_ancillary(control_len);
        let mut message = unsafe { MaybeUninit::<libc::msghdr>::zeroed().assume_init() };
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = ancillary.len() * mem::size_of::<usize>();
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as usize;
            libc::CMSG_DATA(header)
                .cast::<RawFd>()
                .write_unaligned(descriptor);
        }
        assert_eq!(
            unsafe { libc::sendmsg(socket, &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }

    fn parsed(needed: &[&str], soname: Option<&str>) -> ParsedElf {
        ParsedElf {
            interpreter: None,
            has_dynamic: true,
            needed: needed.iter().map(OsString::from).collect(),
            soname: soname.map(OsString::from),
            has_injection_tags: false,
        }
    }

    fn bootstrap() -> BootstrapSpec {
        BootstrapSpec {
            session: [7; 32],
            release: [9; 32],
            executable: FileIdentity {
                dev: 1,
                ino: 2,
                size: 3,
            },
            build_marker: [11; 32],
            runtime_objects: vec![RuntimeObject {
                path: PathBuf::from("/dev/.nub-sandbox/runtime/nub-monitor"),
                identity: FileIdentity {
                    dev: 1,
                    ino: 2,
                    size: 3,
                },
            }],
            program: OsString::from_vec(b"/bin/probe\xff".to_vec()),
            args: vec![OsString::from_vec(b"arg\xfe".to_vec())],
            cwd: PathBuf::from(OsString::from_vec(b"/tmp/cwd\xfd".to_vec())),
            env: BTreeMap::from([(
                OsString::from_vec(b"KEY\xfc".to_vec()),
                OsString::from_vec(b"VALUE\xfb".to_vec()),
            )]),
            network_filter: true,
            per_host: false,
            deny_keyring: true,
            permit_keyring_join: false,
            hold_before_initial_stop_for_harness: false,
            hold_after_exec_for_harness: false,
            hold_before_runtime_cleanup_for_harness: false,
            hold_after_runtime_cleanup_for_harness: false,
            hold_after_target_exited_for_harness: false,
            fail_outer_status_after_cleanup_for_harness: false,
            hold_after_completion_request_for_harness: false,
        }
    }

    #[test]
    fn bootstrap_round_trips_non_unicode_bytes() {
        let value = bootstrap();
        assert_eq!(
            BootstrapSpec::decode(&value.encode().unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn bootstrap_round_trips_harness_initial_stop_fault() {
        let mut value = bootstrap();
        value.hold_before_initial_stop_for_harness = true;
        assert_eq!(
            BootstrapSpec::decode(&value.encode().unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn bootstrap_round_trips_sealed_post_exec_harness_hold() {
        let mut value = bootstrap();
        value.hold_after_exec_for_harness = true;
        assert_eq!(
            BootstrapSpec::decode(&value.encode().unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn bootstrap_round_trips_sealed_runtime_cleanup_harness_holds() {
        let mut value = bootstrap();
        value.hold_before_runtime_cleanup_for_harness = true;
        value.hold_after_runtime_cleanup_for_harness = true;
        value.hold_after_target_exited_for_harness = true;
        value.fail_outer_status_after_cleanup_for_harness = true;
        value.hold_after_completion_request_for_harness = true;
        assert_eq!(
            BootstrapSpec::decode(&value.encode().unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn target_exited_report_preserves_exact_terminal_status() {
        for report in [
            TargetExitedReport {
                raw_status: 143 << 8,
                descendants_reaped: 0,
            },
            TargetExitedReport {
                raw_status: libc::SIGTERM,
                descendants_reaped: 27,
            },
            TargetExitedReport {
                raw_status: libc::SIGSEGV | 0x80,
                descendants_reaped: 1,
            },
        ] {
            assert_eq!(
                TargetExitedReport::decode(&report.encode().unwrap()).unwrap(),
                report
            );
        }
        for malformed in [
            -1,
            (libc::SIGSTOP << 8) | 0x7f,
            0xffff,
            1 << 20,
            65,
            0x80,
            0x100 | libc::SIGTERM,
            libc::SIGSTOP,
            libc::SIGCONT,
            libc::SIGCHLD,
            libc::SIGURG,
            libc::SIGWINCH,
            libc::SIGTERM | 0x80,
        ] {
            let mut payload = malformed.to_le_bytes().to_vec();
            payload.extend_from_slice(&0u32.to_le_bytes());
            assert!(
                TargetExitedReport::decode(&payload).is_err(),
                "{malformed:#x}"
            );
        }
        assert!(TargetExitedReport::decode(&[0; TARGET_EXITED_LEN - 1]).is_err());
    }

    #[test]
    fn completion_attestation_binds_exact_report_and_capability() {
        let attestation = CompletionAttestation {
            report: TargetExitedReport {
                raw_status: libc::SIGTERM,
                descendants_reaped: 19,
            },
            challenge: [0xa7; COMPLETION_CHALLENGE_LEN],
        };
        let encoded = attestation.encode().unwrap();
        assert_eq!(encoded.len(), COMPLETION_ATTESTATION_LEN);
        assert_eq!(
            CompletionAttestation::decode(&encoded).unwrap(),
            attestation
        );
        attestation.require_exact_echo(&encoded).unwrap();

        let mut wrong_report = encoded.clone();
        wrong_report[4] ^= 1;
        assert!(attestation.require_exact_echo(&wrong_report).is_err());
        let mut wrong_capability = encoded.clone();
        wrong_capability[TARGET_EXITED_LEN] ^= 1;
        assert!(attestation.require_exact_echo(&wrong_capability).is_err());
        assert!(CompletionAttestation::decode(&encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn signal_relay_records_are_packetized_and_sequence_bound() {
        let record = SignalRelayRecord {
            sequence: 4,
            signal: libc::SIGCONT,
        };
        let encoded = record.encode().unwrap();
        assert_eq!(SignalRelayRecord::decode(&encoded, 4).unwrap(), record);
        assert!(SignalRelayRecord::decode(&encoded, 3).is_err());
        assert!(SignalRelayRecord::decode(&encoded[..encoded.len() - 1], 4).is_err());
        let mut bad_magic = encoded;
        bad_magic[0] ^= 0xff;
        assert!(SignalRelayRecord::decode(&bad_magic, 4).is_err());
        assert!(
            SignalRelayRecord {
                sequence: 4,
                signal: libc::SIGKILL,
            }
            .encode()
            .is_err()
        );

        let (reader, mut writer) = signal_relay_pipe_files().unwrap();
        let first = SignalRelayRecord {
            sequence: 0,
            signal: libc::SIGINT,
        }
        .encode()
        .unwrap();
        let second = SignalRelayRecord {
            sequence: 1,
            signal: libc::SIGTERM,
        }
        .encode()
        .unwrap();
        writer.write_all(&first).unwrap();
        writer.write_all(&second).unwrap();
        assert_eq!(
            receive_signal_relay_record(reader.as_raw_fd(), 0, Instant::now() + DESCRIBE_TIMEOUT,)
                .unwrap()
                .unwrap()
                .signal,
            libc::SIGINT
        );
        assert_eq!(
            receive_signal_relay_record(reader.as_raw_fd(), 1, Instant::now() + DESCRIBE_TIMEOUT,)
                .unwrap()
                .unwrap()
                .signal,
            libc::SIGTERM
        );

        let (reader, mut writer) = signal_relay_pipe_files().unwrap();
        let mut trailing = first.to_vec();
        trailing.push(0);
        writer.write_all(&trailing).unwrap();
        assert!(
            receive_signal_relay_record(reader.as_raw_fd(), 0, Instant::now() + DESCRIBE_TIMEOUT,)
                .is_err()
        );
    }

    #[test]
    fn production_signal_relay_serializes_callbacks_and_poison_is_permanent() {
        let (reader, writer) = signal_relay_pipe_files().unwrap();
        let relay = SignalRelay::new(writer);
        let callback = relay.callback();
        callback(libc::SIGINT).unwrap();
        callback(libc::SIGTERM).unwrap();
        assert_eq!(
            receive_signal_relay_record(reader.as_raw_fd(), 0, Instant::now() + DESCRIBE_TIMEOUT,)
                .unwrap()
                .unwrap(),
            SignalRelayRecord {
                sequence: 0,
                signal: libc::SIGINT,
            }
        );
        assert_eq!(
            receive_signal_relay_record(reader.as_raw_fd(), 1, Instant::now() + DESCRIBE_TIMEOUT,)
                .unwrap()
                .unwrap(),
            SignalRelayRecord {
                sequence: 1,
                signal: libc::SIGTERM,
            }
        );

        let first = relay.forward(libc::SIGKILL).unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::BrokenPipe);
        assert!(first.to_string().contains("failed permanently"));
        let second = relay.forward(libc::SIGCONT).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn production_signal_relay_disarm_keeps_writer_until_terminal_close() {
        let (reader, writer) = signal_relay_pipe_files().unwrap();
        let relay = SignalRelay::new(writer);
        relay.disarm();
        let retired = relay.forward(libc::SIGTERM).unwrap_err();
        assert_eq!(retired.kind(), io::ErrorKind::NotConnected);
        assert_eq!(retired.to_string(), "sandbox signal relay is retired");
        assert!(
            relay.writer_retained(),
            "disarm must retire forwarding without releasing the write end"
        );
        assert_eq!(
            receive_signal_relay_record(reader.as_raw_fd(), 0, Instant::now() + DESCRIBE_TIMEOUT,)
                .unwrap(),
            None,
            "a disarmed relay must not have emitted a record"
        );
        relay.close();
        assert!(
            !relay.writer_retained(),
            "close is terminal and must release the write end"
        );
    }

    #[test]
    fn private_runtime_root_is_reserved_below_fresh_dev() {
        assert_eq!(PRIVATE_RUNTIME_ROOT, "/dev/.nub-sandbox/runtime");
        assert!(valid_private_runtime_path(Path::new(
            "/dev/.nub-sandbox/runtime/nub-monitor"
        )));
        assert!(!valid_private_runtime_path(Path::new(
            "/run/other/nub-monitor"
        )));
    }

    #[test]
    fn monitor_exec_environment_accepts_only_stock_bwrap_pwd() {
        assert!(valid_monitor_exec_environment(b""));
        assert!(valid_monitor_exec_environment(b"PWD=/\0"));
        assert!(!valid_monitor_exec_environment(b"PWD=/tmp\0"));
        assert!(!valid_monitor_exec_environment(b"PWD=/\0PATH=/bin\0"));
        assert!(!valid_monitor_exec_environment(b"PWD=/\0\0"));
    }

    #[test]
    fn proc_status_state_ignores_the_state_field_label() {
        assert_eq!(
            proc_status_state(b"Name:\tprobe\nState:\tS (sleeping)\n"),
            Some(b'S')
        );
        assert_eq!(
            proc_status_state(b"Name:\tprobe\nState:\tT (stopped)\n"),
            Some(b'T')
        );
        assert_eq!(proc_status_state(b"Name:\tprobe\n"), None);
    }

    #[test]
    fn target_exec_vectors_are_complete_before_fork() {
        let value = bootstrap();
        let prepared = PreparedTargetExec::new(&value).unwrap();
        assert_eq!(prepared.argv.len(), value.args.len() + 2);
        assert!(prepared.argv.last().unwrap().is_null());
        assert_eq!(prepared.envp.len(), value.env.len() + 1);
        assert!(prepared.envp.last().unwrap().is_null());
        assert_eq!(prepared.program.as_bytes(), value.program.as_bytes());
        assert_eq!(prepared.cwd.as_bytes(), value.cwd.as_os_str().as_bytes());
        assert_eq!(
            prepared._argv_storage[0].as_bytes(),
            value.program.as_bytes()
        );
        assert_eq!(
            prepared._argv_storage[1].as_bytes(),
            value.args[0].as_bytes()
        );
        assert_eq!(prepared._env_storage[0].as_bytes(), b"KEY\xfc=VALUE\xfb");
    }

    #[test]
    fn bootstrap_rejects_truncation_trailing_bytes_and_duplicate_env() {
        let encoded = bootstrap().encode().unwrap();
        for len in 0..encoded.len() {
            assert!(BootstrapSpec::decode(&encoded[..len]).is_err(), "len={len}");
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(BootstrapSpec::decode(&trailing).is_err());

        let mut duplicate = bootstrap();
        duplicate.env = BTreeMap::from([
            (OsString::from("AKEY"), OsString::from("one")),
            (OsString::from("BKEY"), OsString::from("two")),
        ]);
        let mut bytes = duplicate.encode().unwrap();
        let offset = bytes
            .windows(4)
            .position(|window| window == b"BKEY")
            .expect("encoded second environment key");
        bytes[offset..offset + 4].copy_from_slice(b"AKEY");
        assert!(BootstrapSpec::decode(&bytes).is_err());
    }

    #[test]
    fn frame_round_trip_and_rejects_wrong_session_kind_and_length() {
        let frame = Frame {
            session: [3; 32],
            sequence: 42,
            kind: FrameKind::TargetStopped,
            payload: vec![1, 2, 3],
        };
        let encoded = frame.encode().unwrap();
        assert_eq!(Frame::decode(&encoded, &[3; 32]).unwrap(), frame);
        assert!(Frame::decode(&encoded, &[4; 32]).is_err());
        let mut kind = encoded.clone();
        kind[10..12].copy_from_slice(&999u16.to_le_bytes());
        assert!(Frame::decode(&kind, &[3; 32]).is_err());
        let mut length = encoded;
        length[20..24].copy_from_slice(&99u32.to_le_bytes());
        assert!(Frame::decode(&length, &[3; 32]).is_err());
    }

    #[test]
    fn frame_rejects_every_truncation() {
        let encoded = Frame {
            session: [1; 32],
            sequence: 1,
            kind: FrameKind::MonitorReady,
            payload: vec![5; 17],
        }
        .encode()
        .unwrap();
        for len in 0..encoded.len() {
            assert!(Frame::decode(&encoded[..len], &[1; 32]).is_err());
        }
    }

    #[test]
    fn stopped_target_attestation_round_trips_and_rejects_invalid_identity() {
        let attestation = TargetStoppedAttestation {
            namespace_pid: 2,
            starttime: 12345,
            start_challenge: [7; START_CHALLENGE_LEN],
        };
        assert_eq!(
            TargetStoppedAttestation::decode(&attestation.encode()).unwrap(),
            attestation
        );
        for len in 0..TargetStoppedAttestation::ENCODED_LEN {
            assert!(TargetStoppedAttestation::decode(&attestation.encode()[..len]).is_err());
        }
        assert!(
            TargetStoppedAttestation::decode(
                &TargetStoppedAttestation {
                    namespace_pid: 1,
                    starttime: 12345,
                    start_challenge: [7; START_CHALLENGE_LEN],
                }
                .encode()
            )
            .is_err()
        );
        assert!(
            TargetStoppedAttestation::decode(
                &TargetStoppedAttestation {
                    namespace_pid: 2,
                    starttime: 0,
                    start_challenge: [7; START_CHALLENGE_LEN],
                }
                .encode()
            )
            .is_err()
        );
    }

    #[test]
    fn target_exec_failure_round_trips_and_rejects_malformed_payloads() {
        let failure = TargetExecFailure {
            stage: TargetSetupStage::Execve,
            errno: libc::ENOENT,
            raw_status: 126 << 8,
        };
        assert_eq!(
            TargetExecFailure::decode(&failure.encode()).unwrap(),
            failure
        );
        for len in 0..TARGET_EXEC_FAILURE_LEN {
            assert!(TargetExecFailure::decode(&failure.encode()[..len]).is_err());
        }
        let mut trailing = failure.encode();
        trailing.push(0);
        assert!(TargetExecFailure::decode(&trailing).is_err());
        let mut wrong_stage = failure.encode();
        wrong_stage[..2].copy_from_slice(&(TargetSetupStage::DescriptorSweep as u16).to_le_bytes());
        assert!(TargetExecFailure::decode(&wrong_stage).is_err());
        let mut reserved = failure.encode();
        reserved[2] = 1;
        assert!(TargetExecFailure::decode(&reserved).is_err());
        let mut zero_errno = failure.encode();
        zero_errno[4..8].copy_from_slice(&0i32.to_le_bytes());
        assert!(TargetExecFailure::decode(&zero_errno).is_err());
        let mut stopped_status = failure.encode();
        stopped_status[8..12].copy_from_slice(&(libc::SIGSTOP << 8 | 0x7f).to_le_bytes());
        assert!(TargetExecFailure::decode(&stopped_status).is_err());
    }

    #[test]
    fn target_setup_error_transport_names_the_exact_stage_and_errno() {
        let (reader, mut writer) = pipe_files(false).unwrap();
        let mut record = [0u8; TARGET_SETUP_ERROR_LEN];
        record[..4].copy_from_slice(TARGET_SETUP_ERROR_MAGIC);
        record[4..6].copy_from_slice(&(TargetSetupStage::DescriptorSweep as u16).to_le_bytes());
        record[8..12].copy_from_slice(&libc::EMFILE.to_le_bytes());
        writer.write_all(&record).unwrap();
        drop(writer);
        let error = read_target_setup_error(reader.as_raw_fd(), 126 << 8);
        let message = error.to_string();
        assert!(message.contains("descriptor-sweep"));
        assert!(message.contains(&io::Error::from_raw_os_error(libc::EMFILE).to_string()));

        let (reader, mut writer) = pipe_files(false).unwrap();
        writer.write_all(&record[..record.len() - 1]).unwrap();
        drop(writer);
        assert!(
            read_target_setup_error(reader.as_raw_fd(), 126 << 8)
                .to_string()
                .contains("truncated")
        );
    }

    #[test]
    fn target_old_kernel_fd_fallback_closes_sparse_descriptors() {
        let secret = tempfile::tempfile().unwrap();
        let mut limit = MaybeUninit::<libc::rlimit>::uninit();
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
            0
        );
        let limit = unsafe { limit.assume_init() };
        let descriptor_ceiling = limit.rlim_cur.min(limit.rlim_max).min(8192);
        assert!(descriptor_ceiling > 256);
        let high_floor = (descriptor_ceiling / 2).max(128) as RawFd;
        assert!(high_floor > 64);
        let high_fd = unsafe { libc::fcntl(secret.as_raw_fd(), libc::F_DUPFD_CLOEXEC, high_floor) };
        assert!(high_fd >= high_floor);
        let high = unsafe { File::from_raw_fd(high_fd) };
        let (mut result_reader, result_writer) = pipe_files(false).unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "{}", io::Error::last_os_error());
        if pid == 0 {
            let result_fd = result_writer.as_raw_fd();
            let closed = unsafe { child_close_open_fds_from_proc(&[result_fd]) }.is_ok()
                && unsafe { libc::fcntl(secret.as_raw_fd(), libc::F_GETFD) } < 0
                && child_errno() == libc::EBADF
                && unsafe { libc::fcntl(high.as_raw_fd(), libc::F_GETFD) } < 0
                && child_errno() == libc::EBADF;
            let byte = u8::from(closed);
            let _ = unsafe { libc::write(result_fd, (&byte as *const u8).cast(), 1) };
            unsafe { libc::_exit(if closed { 0 } else { 1 }) };
        }
        drop(result_writer);
        let mut byte = [0u8; 1];
        result_reader.read_exact(&mut byte).unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(byte, [1]);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    #[test]
    fn credentialed_control_channel_rejects_replay() {
        let (left, right) = owned_pair(libc::SOCK_SEQPACKET);
        let peer = ExpectedPeer {
            pid: Some(unsafe { libc::getpid() }),
            uid: unsafe { libc::getuid() },
        };
        let mut sender = ControlChannel::new(left, [4; 32], peer).unwrap();
        let mut receiver = ControlChannel::new(right, [4; 32], peer).unwrap();
        sender
            .send(FrameKind::MonitorReady, b"ready".to_vec())
            .unwrap();
        let frame = receiver.receive().unwrap();
        assert_eq!(frame.kind, FrameKind::MonitorReady);
        assert_eq!(frame.payload, b"ready");

        let replay = Frame {
            session: [4; 32],
            sequence: 0,
            kind: FrameKind::MonitorReady,
            payload: Vec::new(),
        }
        .encode()
        .unwrap();
        assert_eq!(
            unsafe {
                libc::send(
                    sender.fd.as_raw_fd(),
                    replay.as_ptr().cast(),
                    replay.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            replay.len() as isize
        );
        assert!(receiver.receive().is_err());
    }

    #[test]
    fn deadline_send_is_nonblocking_and_does_not_advance_on_timeout() {
        let (left, _right) = owned_pair(libc::SOCK_SEQPACKET);
        let send_buffer: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    left.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&send_buffer as *const libc::c_int).cast(),
                    mem::size_of_val(&send_buffer) as libc::socklen_t,
                )
            },
            0
        );
        let peer = ExpectedPeer {
            pid: Some(unsafe { libc::getpid() }),
            uid: unsafe { libc::getuid() },
        };
        let mut sender = ControlChannel::new(left, [3; 32], peer).unwrap();
        let filler = [0u8; 1024];
        loop {
            let sent = unsafe {
                libc::send(
                    sender.fd.as_raw_fd(),
                    filler.as_ptr().cast(),
                    filler.len(),
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if sent >= 0 {
                continue;
            }
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
            break;
        }
        let started = Instant::now();
        let error = sender
            .send_with_deadline(
                FrameKind::TargetExited,
                Vec::new(),
                Some(started + Duration::from_millis(25)),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(sender.send_sequence, 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn control_channel_rejects_wrong_peer_eof_and_descriptors() {
        let session = [6; 32];
        let (sender, receiver) = owned_pair(libc::SOCK_SEQPACKET);
        let mut receiver = ControlChannel::new(
            receiver,
            session,
            ExpectedPeer {
                pid: Some(unsafe { libc::getpid() } + 1),
                uid: unsafe { libc::getuid() },
            },
        )
        .unwrap();
        let frame = Frame {
            session,
            sequence: 0,
            kind: FrameKind::StartTarget,
            payload: Vec::new(),
        }
        .encode()
        .unwrap();
        assert_eq!(
            unsafe {
                libc::send(
                    sender.as_raw_fd(),
                    frame.as_ptr().cast(),
                    frame.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            frame.len() as isize
        );
        assert!(receiver.receive().is_err());

        let (sender, receiver) = owned_pair(libc::SOCK_SEQPACKET);
        let mut receiver = ControlChannel::new(
            receiver,
            session,
            ExpectedPeer {
                pid: Some(unsafe { libc::getpid() }),
                uid: unsafe { libc::getuid() },
            },
        )
        .unwrap();
        drop(sender);
        assert_eq!(
            receiver.receive().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let (sender, receiver) = owned_pair(libc::SOCK_SEQPACKET);
        let mut receiver = ControlChannel::new(
            receiver,
            session,
            ExpectedPeer {
                pid: Some(unsafe { libc::getpid() }),
                uid: unsafe { libc::getuid() },
            },
        )
        .unwrap();
        let null = File::open("/dev/null").unwrap();
        send_with_descriptor(sender.as_raw_fd(), &frame, null.as_raw_fd());
        assert!(receiver.receive().is_err());
    }

    #[test]
    fn fatal_transition_is_an_authenticated_frame() {
        let (monitor, parent) = owned_pair(libc::SOCK_SEQPACKET);
        let peer = ExpectedPeer {
            pid: Some(unsafe { libc::getpid() }),
            uid: unsafe { libc::getuid() },
        };
        let mut monitor = ControlChannel::new(monitor, [5; 32], peer).unwrap();
        let mut parent = ControlChannel::new(parent, [5; 32], peer).unwrap();
        send_fatal(&mut monitor, &invalid_data("fatal-test"));
        let frame = parent.receive().unwrap();
        assert_eq!(frame.kind, FrameKind::Fatal);
        assert_eq!(frame.payload, b"fatal-test");
    }

    #[test]
    fn sealed_bootstrap_descriptor_is_required_and_bounded() {
        let expected = bootstrap();
        let bytes = expected.encode().unwrap();
        let sealed = memfd_with(&bytes, true);
        assert_eq!(read_sealed_bootstrap(sealed.as_raw_fd()).unwrap(), expected);

        let unsealed = memfd_with(&bytes, false);
        assert!(read_sealed_bootstrap(unsealed.as_raw_fd()).is_err());

        let oversized = memfd_with(&[], false);
        assert_eq!(
            unsafe {
                libc::ftruncate(
                    oversized.as_raw_fd(),
                    (MAX_BOOTSTRAP_BYTES + 1) as libc::off_t,
                )
            },
            0
        );
        assert_eq!(
            unsafe {
                libc::fcntl(
                    oversized.as_raw_fd(),
                    libc::F_ADD_SEALS,
                    REQUIRED_BOOTSTRAP_SEALS,
                )
            },
            0
        );
        assert!(read_sealed_bootstrap(oversized.as_raw_fd()).is_err());
    }

    #[test]
    fn release_gate_requires_exact_capability_and_eof() {
        let expected = [8; 32];
        let valid = release_pipe(&expected);
        read_exact_capability_and_eof(valid.as_raw_fd(), &expected).unwrap();

        let wrong = release_pipe(&[7; 32]);
        assert!(read_exact_capability_and_eof(wrong.as_raw_fd(), &expected).is_err());

        let mut trailing = expected.to_vec();
        trailing.push(1);
        let trailing = release_pipe(&trailing);
        assert!(read_exact_capability_and_eof(trailing.as_raw_fd(), &expected).is_err());
    }

    #[test]
    fn monitor_states_through_target_exec_are_linear() {
        let mut state = MonitorState::Bootstrap;
        transition_monitor_state(
            &mut state,
            MonitorState::Bootstrap,
            MonitorState::MonitorReadyAwaitingRelease,
        )
        .unwrap();
        assert_eq!(state, MonitorState::MonitorReadyAwaitingRelease);
        assert!(
            transition_monitor_state(
                &mut state,
                MonitorState::Bootstrap,
                MonitorState::MonitorReleased,
            )
            .is_err()
        );
        assert_eq!(state, MonitorState::MonitorReadyAwaitingRelease);
        transition_monitor_state(
            &mut state,
            MonitorState::MonitorReadyAwaitingRelease,
            MonitorState::MonitorReleased,
        )
        .unwrap();
        transition_monitor_state(
            &mut state,
            MonitorState::MonitorReleased,
            MonitorState::TargetStopped,
        )
        .unwrap();
        assert_eq!(state, MonitorState::TargetStopped);
        assert!(
            transition_monitor_state(
                &mut state,
                MonitorState::MonitorReleased,
                MonitorState::TargetStopped,
            )
            .is_err()
        );
        transition_monitor_state(
            &mut state,
            MonitorState::TargetStopped,
            MonitorState::TargetStarting,
        )
        .unwrap();
        transition_monitor_state(
            &mut state,
            MonitorState::TargetStarting,
            MonitorState::TargetRunning,
        )
        .unwrap();
        assert_eq!(state, MonitorState::TargetRunning);
        transition_monitor_state(
            &mut state,
            MonitorState::TargetRunning,
            MonitorState::TargetExitedAwaitingCompletion,
        )
        .unwrap();
        assert_eq!(state, MonitorState::TargetExitedAwaitingCompletion);
        transition_monitor_state(
            &mut state,
            MonitorState::TargetExitedAwaitingCompletion,
            MonitorState::SessionCompletionAuthorized,
        )
        .unwrap();
        transition_monitor_state(
            &mut state,
            MonitorState::SessionCompletionAuthorized,
            MonitorState::CleanupCompletePublished,
        )
        .unwrap();
        assert_eq!(state, MonitorState::CleanupCompletePublished);

        let mut failed = MonitorState::TargetStarting;
        transition_monitor_state(
            &mut failed,
            MonitorState::TargetStarting,
            MonitorState::TargetStartFailed,
        )
        .unwrap();
        assert_eq!(failed, MonitorState::TargetStartFailed);
    }

    #[test]
    fn bootstrap_rejects_nul_and_equal_environment_keys() {
        let mut value = bootstrap();
        value.env = BTreeMap::from([(OsString::from("AKEY"), OsString::from("value"))]);
        let mut malformed = value.encode().unwrap();
        let offset = malformed
            .windows(4)
            .position(|window| window == b"AKEY")
            .expect("encoded environment key");
        malformed[offset..offset + 4].copy_from_slice(b"A=EY");
        assert!(BootstrapSpec::decode(&malformed).is_err());
        value
            .env
            .insert(OsString::from("BAD=KEY"), OsString::from("value"));
        assert!(value.encode().is_err());
        value.env.clear();
        value.program = OsString::from_vec(b"bad\0program".to_vec());
        assert!(value.encode().is_err());
    }

    #[test]
    fn bootstrap_rejects_runtime_paths_outside_the_private_image() {
        let mut value = bootstrap();
        value.runtime_objects[0].path = PathBuf::from("/etc/passwd");
        assert!(value.encode().is_err());

        let mut value = bootstrap();
        value.runtime_objects.push(value.runtime_objects[0].clone());
        assert!(value.encode().is_err());
    }

    #[test]
    fn maps_record_preserves_path_bytes_and_parses_device_identity() {
        let line = b"7f00-8000 r-xp 00000000 fd:01 6277                       /tmp/lib with space\\012and-newline\xff.so";
        let record = parse_maps_record(line).unwrap().unwrap();
        assert_eq!(record.device, (0xfd, 1));
        assert_eq!(record.inode, 6277);
        assert_eq!(
            decode_maps_path(record.path),
            b"/tmp/lib with space\nand-newline\xff.so"
        );
    }

    #[test]
    fn mapped_inventory_retains_the_open_file_after_path_removal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mapped-object");
        fs::write(&path, b"mapped bytes").unwrap();
        let file = File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        let device = (
            libc::major(identity.dev as libc::dev_t),
            libc::minor(identity.dev as libc::dev_t),
        );
        let maps = format!(
            "1000-2000 r--p 00000000 {:x}:{:x} {} {}",
            device.0,
            device.1,
            identity.ino,
            path.display()
        );
        let inventory = pinned_inventory_from_maps(maps.as_bytes()).unwrap();
        assert_eq!(inventory.len(), 1);
        fs::remove_file(&path).unwrap();
        let mut bytes = Vec::new();
        (&inventory[0].file).read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"mapped bytes");
    }

    #[test]
    fn runtime_marker_is_order_independent_and_detects_manifest_tampering() {
        let first = RuntimeObject {
            path: PathBuf::from("/dev/.nub-sandbox/runtime/nub-monitor"),
            identity: FileIdentity {
                dev: 1,
                ino: 2,
                size: 3,
            },
        };
        let second = RuntimeObject {
            path: PathBuf::from("/dev/.nub-sandbox/runtime/ld.so"),
            identity: FileIdentity {
                dev: 4,
                ino: 5,
                size: 6,
            },
        };
        let marker = runtime_build_marker(&[first.clone(), second.clone()]);
        assert_eq!(
            marker,
            runtime_build_marker(&[second.clone(), first.clone()])
        );
        validate_runtime_build_marker(&[second.clone(), first.clone()], &marker).unwrap();
        let mut tampered = second;
        tampered.identity.ino += 1;
        let tampered = [first, tampered];
        assert_ne!(marker, runtime_build_marker(&tampered));
        assert!(validate_runtime_build_marker(&tampered, &marker).is_err());
    }

    #[test]
    fn bounded_runtime_inputs_reject_overflow_during_the_read() {
        let mut output = Vec::new();
        extend_description_output(&mut output, &[0; 1024], 1024).unwrap();
        assert!(extend_description_output(&mut output, &[0], 1024).is_err());
        assert!(read_bounded(io::Cursor::new([0; 17]), 16, "test input").is_err());
    }

    #[test]
    fn no_new_privs_status_allows_the_pre_4_10_absent_field_only() {
        assert!(valid_no_new_privs_status(None));
        assert!(valid_no_new_privs_status(Some("1")));
        assert!(!valid_no_new_privs_status(Some("0")));
        assert!(!valid_no_new_privs_status(Some("garbage")));
    }

    #[test]
    fn musl_runtime_fails_closed_until_loader_search_is_proven() {
        let family = loader_family(Path::new("/lib/ld-musl-aarch64.so.1")).unwrap();
        assert_eq!(family, LoaderFamily::Musl);
        let error = require_proven_loader_search(family).unwrap_err();
        assert!(error.to_string().contains("monitor-runtime-musl-search"));
        require_proven_loader_search(LoaderFamily::Glibc).unwrap();
    }

    #[test]
    fn runtime_closure_preserves_every_needed_alias_for_one_inode() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        let basename = path.file_name().unwrap().to_owned();
        let identity = FileIdentity::from_file(file.as_file()).unwrap();
        let inventory = [InventoryObject {
            file: duplicate_above_stdio(file.as_file()).unwrap(),
            path,
            aliases: BTreeSet::from([basename.clone()]),
            identity,
            parsed: parsed(&[], Some("libalias.so")),
        }];
        let root = ParsedElf {
            needed: vec![basename.clone(), OsString::from("libalias.so")],
            ..parsed(&[], None)
        };

        let (objects, _) = resolve_needed_closure(&[&root], &inventory).unwrap();
        let names = objects
            .iter()
            .map(|object| object.private_name.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from([basename, OsString::from("libalias.so")])
        );
        assert!(objects.iter().all(|object| object.identity == identity));
    }

    #[test]
    fn ordinary_host_bootstrap_pins_authority_but_defers_full_runtime_image() {
        let runtime = earliest_bootstrap().unwrap();
        match &runtime.source {
            RuntimeSource::Current { authority, image } => {
                let authority = authority
                    .as_ref()
                    .expect("host bootstrap captured authority");
                assert!(image.get().is_none());
                assert!(!authority.inventory.is_empty());
                assert!(
                    authority
                        .inventory
                        .iter()
                        .all(|candidate| candidate.file.as_raw_fd() >= 3)
                );
            }
            RuntimeSource::Explicit(_) => panic!("ordinary bootstrap returned explicit runtime"),
        }
    }

    #[test]
    fn unconfined_apply_does_not_materialize_the_host_token() {
        let runtime = RuntimeCapability::current_process();
        let mut policy = SandboxPolicy::default();
        policy.env.resolved = true;
        policy.fs.rules.default_effect = crate::policy::Effect::Allow;
        let _prepared = crate::backend::apply_with_runtime(
            &policy,
            CommandSpec::new("/usr/bin/true"),
            &runtime,
        )
        .unwrap();
        match &runtime.source {
            RuntimeSource::Current { image, .. } => assert!(image.get().is_none()),
            RuntimeSource::Explicit(_) => panic!("host token changed authority kind"),
        }
    }
}
