//! Linux backend built around Bubblewrap's private filesystem and PID views.
//!
//! Bubblewrap constructs the view; it does not copy project files. Read-only and
//! writable binds keep their original absolute paths and write directly to the host.
//! Exact deny paths are layered last so a writable project cannot re-expose them.
#![cfg(target_os = "linux")]

use crate::backend::linux_grants::{self, MountAccess, MountGrant, fs_confines};
use crate::backend::{CommandSpec, Degradation, Prepared};
use crate::matcher::path::{PathMatcher, canonicalize_including_nonexistent};
use crate::policy::{Effect, FsAccess, SandboxPolicy, TmpMode};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch, sock_filter,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command};
use std::time::{Duration, Instant};

/// The system read floor a `RootView::Minimal` sandbox mounts before any authored grant:
/// what a process needs to START, not what it might find useful. Every entry is
/// package-manager-owned and world-readable; an absent one is skipped, so this is a
/// superset that degrades to whatever the host actually has.
///
/// `/usr` `/bin` `/sbin` `/lib*` stay WHOLESALE deliberately. A native build is not 39
/// binaries, it is a compiler: `cc1plus`/`collect2` live under a version- and
/// triple-keyed `/usr/lib/gcc/<triple>/<major>/`, and GCC OPENS far more than it execs —
/// `specs`, `crt*.o`, the linker scripts, and the whole `/usr/include/**` header tree
/// (one `node-gyp rebuild` measured 595 opens under `/usr/include` alone). A positive
/// per-path grant would have to track every GCC major, arch triple, and distro forever,
/// and buys little: there is no credential class under those roots.
///
/// `/etc` and `/opt` are where the credentials actually are, so they are NOT wholesale.
/// `/etc` is enumerated below; `/opt` is absent entirely — it is third-party software
/// (~11 GB of it on a GitHub Actions runner: `hostedtoolcache`, `az`, `pipx`, `microsoft`,
/// `google`), never a system floor. An interpreter that happens to live under `/opt` is
/// unaffected: it is bound by its own policy grant or as the entry program below, never
/// by this floor.
///
/// The `/etc` set is MEASURED, not guessed (`.fray/sandbox-minimum-readset.md` §5.5): a
/// 34-package real-postinstall corpus was re-run under an EMPTY `/etc` with
/// `strace -e trace=%file`, so these are the paths the kernel was actually asked for —
/// the loader, NSS, DNS, timezone and TLS floor.
///
/// The TLS entries are DISTRO-SHAPED and the measured set alone is not portable, because
/// the corpus ran on Debian only. On the RHEL family `/etc/ssl/certs` is a symlink to
/// `/etc/pki/tls/certs` whose entries are ABSOLUTE symlinks into `/etc/pki/ca-trust/…`,
/// and `OPENSSLDIR` is `/etc/pki/tls` — so binding the Debian paths alone yields a
/// directory of DANGLING symlinks and every OpenSSL verify fails with "unable to get
/// local issuer certificate" (reproduced on `rockylinux:9` against a wholesale-`/etc`
/// control). `/etc/ssl/cert.pem` is musl/Alpine's default `SSL_CERT_FILE`. Node bundles
/// its own CA and is unaffected, which is exactly why a Node-only corpus stayed green —
/// but `curl`, `git clone https://`, and python `requests` inside the jail are not.
///
/// Every TLS entry is named as a SUBPATH, never `/etc/ssl` or `/etc/pki` wholesale:
/// `/etc/ssl/private` and `/etc/pki/tls/private` are mode-700 private-key directories and
/// admitting either would undo the tightening this floor exists for.
pub(super) const ESSENTIAL_READ_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/lib32",
    "/libx32",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/resolv.conf",
    "/etc/localtime",
    "/etc/alternatives",
    // GIT TREATS DENIED AS FATAL AND ABSENT AS FINE, so an ungranted `/etc/gitconfig` is WORSE
    // than no system config at all. `git-compat-util.h` `is_missing_file_error()` is
    // ENOENT‖ENOTDIR only; `wrapper.c` `warn_on_fopen_errors()` returns -1 for anything else;
    // `config.c` turns that into `die("unknown error occurred while reading the configuration
    // files")`. Docker differential on git 2.39.5 and 2.47.3, positive control firing:
    // ABSENT rc=0 · DENIED rc=128 · READABLE rc=0.
    //
    // ⛔ WHY THIS HID FOR SO LONG. macOS ships no `/etc/gitconfig`, so the same packages measure
    // `write:{project}` there and `write:"disk"` here — that ONE FILE is the entire divergence,
    // and it made the tail look like an exec-permission problem it is not (`git` at `/usr/bin`
    // is readable; `/usr` is bound wholesale above). Landlock also hooks `file_open`, NOT
    // `inode_permission`, so git's `access(R_OK)` probe SUCCEEDS and only the later `fopen` is
    // denied — which is why the failure surfaces as a generic "unknown error" naming no path.
    // Silent-degradation note: lefthook exits 0 having written ZERO hooks on this path.
    //
    // World-readable system config, not credential material — same category as the TLS and
    // resolver entries below. `gitattributes` and `git-core` are the same family.
    "/etc/gitconfig",
    "/etc/gitattributes",
    "/etc/git-core",
    // TLS trust material. Debian/SUSE spellings first, then the RHEL family, then musl.
    "/etc/ssl/certs",
    "/etc/ssl/openssl.cnf",
    "/etc/ssl/cert.pem",
    "/etc/ca-certificates",
    "/etc/pki/tls/certs",
    "/etc/pki/tls/openssl.cnf",
    "/etc/pki/ca-trust",
    // RHEL's openssl.cnf `.include`s `/etc/crypto-policies/back-ends/opensslcnf.config`,
    // and OpenSSL treats the missing include as a fatal config error rather than skipping
    // it — so without this every `openssl` invocation dies at startup even with the trust
    // material above already bound.
    "/etc/crypto-policies",
];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootView {
    ReadWrite,
    ReadOnly,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskKind {
    /// Dotenv readers get an empty regular file instead of EACCES.
    EmptyReadable,
    /// Explicit denies remain genuinely unreadable.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mask {
    path: PathBuf,
    kind: MaskKind,
    directory: bool,
    /// Position of the deny rule that produced this mask in `FsRuleSet::entries`, which
    /// is the emitter's interleaving key. [`INFRASTRUCTURE_ORDER`] marks a mask the
    /// BACKEND installs (network-equivalent sockets, alternate procfs, keyring metadata)
    /// rather than one the policy authored; those have no authored position and sort
    /// after every policy operation, which is where they have always been emitted.
    order: usize,
}

/// Sort key for a mask with no authored position. Deliberately the maximum: an
/// infrastructure mask is unconditional, so it must never be layered under — and thereby
/// clobbered by — a policy bind.
const INFRASTRUCTURE_ORDER: usize = usize::MAX;

const FIRST_LAUNCH_DATA_FD: RawFd = super::linux_monitor::SIGNAL_RELAY_FD + 1;
const MAX_BUNDLED_BWRAP_BYTES: u64 = 16 * 1024 * 1024;
const REQUIRED_EXECUTABLE_SNAPSHOT_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

/// The administrator-installed nesting helper. It is the unmodified packaged
/// Bubblewrap, root-owned and group-executable, with a dedicated path-bound
/// `userns` AppArmor profile. It is the ONLY candidate that can launch the
/// outermost sandbox of a nesting chain on an AppArmor-restricted host: a stock
/// `bwrap//&unpriv_bwrap` outer cannot transition to the helper's more permissive
/// profile under `no_new_privs`. See `crates/nub-sandbox/setup/linux-nesting/`.
pub(crate) const DEDICATED_HELPER_PATH: &str = "/usr/libexec/nub/nub-bwrap";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BubblewrapOrigin {
    /// The administrator-installed nesting helper at [`DEDICATED_HELPER_PATH`].
    DedicatedHelper,
    System,
    Bundled,
}

/// One opened Bubblewrap inode.  Selection, admission, and the real launch all
/// execute this descriptor through `/proc/self/fd`; the pathname is diagnostic
/// and nested-sandbox inventory only, never execution authority.
struct PinnedBubblewrapCandidate {
    executable: File,
    source_path: PathBuf,
    source_identity: (u64, u64),
}

impl PinnedBubblewrapCandidate {
    fn program(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.executable.as_raw_fd()))
    }
}

pub(crate) struct LinuxPreflight {
    confined: Option<ConfinedPreflight>,
    /// Set when the Landlock mechanism was selected. Mutually exclusive with `confined`:
    /// Landlock needs no bubblewrap candidate, no runtime image, and no namespace, so the
    /// whole bubblewrap admission path below is SKIPPED rather than attempted and
    /// discarded — which is the point, since that admission is exactly what fails on a
    /// restricted-userns host.
    landlock: Option<LandlockPreflight>,
}

impl LinuxPreflight {
    /// Whether this launch will take the Landlock arm. Read by [`super::apply_inner`] BEFORE it
    /// starts the egress proxy, because this mechanism can never route a child through one.
    pub(crate) fn uses_landlock(&self) -> bool {
        self.landlock.is_some()
    }
}

struct LandlockPreflight {
    abi: u32,
}

struct ConfinedPreflight {
    root_view: RootView,
    cwd: PathBuf,
    entry_program: PathBuf,
    mount_plan: Vec<MountGrant>,
    masks: Vec<Mask>,
    bwrap: PinnedBubblewrapCandidate,
    ca_placeholder: Option<File>,
}

/// Apply a resolved policy using Bubblewrap. The exact same operation can be nested:
/// an outer mount/PID view remains in force and the child adds a stricter view inside it.
pub(crate) fn preflight(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
    runtime: Option<&super::linux_monitor::RuntimeCapability>,
) -> Result<LinuxPreflight, Degradation> {
    validate_process_inputs(spec).map_err(|reason| Degradation {
        lost: vec!["process-input".to_string()],
        reason: Some(reason),
    })?;
    let confine_fs = fs_confines(&policy.fs);
    let sandboxing =
        confine_fs || policy.net.enforce || policy.env.enforce || policy.fs.tmp != TmpMode::Shared;
    if !sandboxing {
        return Ok(LinuxPreflight {
            confined: None,
            landlock: None,
        });
    }
    // THE BUILD JAIL'S ONLY MECHANISM. There is no bubblewrap arm below this for a build-jail
    // policy — bubblewrap needs a user namespace, which is not universally available
    // unprivileged, and universal unprivileged operation is what defines this product.
    // Landlock or nothing, decided here and nowhere else.
    match super::linux_landlock::landlock_availability(policy) {
        Ok(abi) => {
            return Ok(LinuxPreflight {
                confined: None,
                landlock: Some(LandlockPreflight { abi }),
            });
        }
        // FAIL CLOSED. A dependency's install script is the code the jail exists to contain,
        // so an unconfinable host must refuse it rather than run it unconfined. The one
        // escape is the internal differential pin, which deliberately routes to bubblewrap.
        Err(super::linux_landlock::LandlockUnavailable::PinnedToBubblewrap) => {}
        Err(super::linux_landlock::LandlockUnavailable::NotABuildJail) => {}
        // ⛔ DO NOT BLAME THE KERNEL FOR A POLICY BUG. `LandlockUnavailable` covers two very
        // different failures and this arm used to describe both as a missing kernel feature:
        //
        //   - the kernel genuinely lacks Landlock (pre-5.13, or a container that masks it), and
        //   - `PolicyNotExpressible` — the kernel is fine and OUR policy will not compile.
        //
        // MEASURED: on a 6.17 kernel with Landlock ABI 4, an authored grant naming a path that
        // did not exist produced `PolicyNotExpressible("filesystem mount source does not exist:
        // node")` and this message told the user their kernel was too old. They would go check
        // their kernel version, find it fine, and have nowhere else to look — while the real
        // cause sat in the parenthetical they were being steered away from.
        Err(reason @ super::linux_landlock::LandlockUnavailable::PolicyNotExpressible(_)) => {
            return Err(Degradation {
                lost: vec!["fs".to_string(), "net".to_string()],
                reason: Some(format!(
                    "the dependency build jail could not COMPILE its policy on this host — this \
                     is a nub bug, not a missing kernel feature: {reason:?}"
                )),
            });
        }
        Err(reason) => {
            return Err(Degradation {
                lost: vec!["fs".to_string(), "net".to_string()],
                reason: Some(format!(
                    "the dependency build jail requires Landlock (Linux 5.13+), which this \
                     kernel does not provide: {reason:?}"
                )),
            });
        }
    }
    let runtime = runtime.ok_or_else(|| Degradation {
        lost: vec!["runtime-capability-missing".to_string()],
        reason: Some(
            "Linux sandbox confinement requires the embedder's earliest bootstrap capability"
                .to_string(),
        ),
    })?;
    let runtime_image = runtime
        .materialize()
        .map_err(super::linux_monitor::runtime_degradation)?;
    let root_view = root_view(policy);
    let cwd = spec
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some("resolving inherited sandbox working directory failed".to_string()),
        })?;
    let cwd = fs::canonicalize(&cwd).map_err(|error| Degradation {
        lost: vec!["process-cwd".to_string()],
        reason: Some(format!(
            "resolving sandbox working directory {}: {error}",
            cwd.display()
        )),
    })?;
    if !cwd.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return Err(Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some(format!(
                "sandbox working directory is not a directory: {}",
                cwd.display()
            )),
        });
    }
    let entry_program = resolve_program(&spec.program, &cwd, target_path(policy).as_deref())
        .ok_or_else(|| Degradation {
            lost: vec!["process-entry".to_string()],
            reason: Some(format!(
                "sandbox entry program could not be resolved in the target environment: {}",
                Path::new(&spec.program).display()
            )),
        })?;
    let mut mount_plan =
        linux_grants::compile_mount_plan(policy).map_err(|reason| Degradation {
            lost: vec!["fs-partial".to_string()],
            reason: Some(reason),
        })?;
    cap_inherited_read_only(&mut mount_plan);
    if crate::requires_deny_search_roots(policy) && spec.deny_search_roots.is_empty() {
        return Err(Degradation {
            lost: vec!["fs-read-deny".to_string()],
            reason: Some(
                "bounded deny globs require declared project/workspace search roots".to_string(),
            ),
        });
    }
    let mut masks =
        collect_masks(policy, &spec.deny_search_roots).map_err(|reason| Degradation {
            lost: vec!["fs-read-deny".to_string()],
            reason: Some(reason),
        })?;
    // C3 cross-layer: the empty netns confines IP traffic, but a filesystem-path AF_UNIX
    // socket crosses that boundary — so under net-confinement the known network-equivalent
    // daemon/bus sockets are force-masked here, making net confinement SELF-CONTAINED
    // rather than dependent on the fs policy happening to hide them.
    if policy.net.enforce {
        masks.extend(net_equivalent_socket_masks().map_err(|reason| Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(reason),
        })?);
    }
    // A nesting-capable launch must keep /proc FULLY VISIBLE: the kernel refuses a
    // NESTED procfs mount when the template /proc carries a locked bind over a procfs
    // FILE, so a child inside this sandbox could not create its own PID-isolated
    // procfs view. The keyring /proc/keys + /proc/key-users metadata masks are exactly
    // such binds, so they are skipped here when nesting is required. Credential
    // protection at a nesting level still rests on the keyctl seccomp deny (the
    // anonymous session-keyring join stays permitted) and the anonymous keyring itself
    // — only the metadata-read defense-in-depth layer is traded for nestability. A
    // single-level launch (require_nesting=false) keeps the full mask, byte-identical.
    if protects_ambient_credentials(policy) && !spec.require_nesting {
        masks.extend(keyring_procfs_masks().map_err(|reason| Degradation {
            lost: vec!["env".to_string()],
            reason: Some(reason),
        })?);
    }
    masks.extend(alternate_procfs_masks().map_err(|reason| Degradation {
        lost: vec!["proc".to_string()],
        reason: Some(reason),
    })?);
    masks = merge_masks(masks);
    if spec.require_nesting {
        // A mask WITHIN /proc breaks fs_fully_visible and would block a nested child's
        // procfs mount. The keyring metadata masks are already skipped for nesting
        // above, so any mask under /proc here is a USER-authored `!/proc/...` deny. Do
        // NOT silently drop a user-requested restriction: fail CLOSED (fail-safe) so the
        // caller sees the incompatibility, rather than launch with the deny quietly
        // gone. Alternate-procfs masks (outside /proc) never reach this branch.
        let proc_root = Path::new("/proc");
        if let Some(mask) = masks
            .iter()
            .find(|mask| mask.path == proc_root || mask.path.starts_with("/proc/"))
        {
            return Err(Degradation {
                lost: vec!["fs-read-deny".to_string()],
                reason: Some(format!(
                    "a nesting sandbox cannot also deny a path under /proc ({}): the kernel requires /proc fully visible so a nested child can mount its own procfs — remove the /proc deny or disable nesting",
                    mask.path.display()
                )),
            });
        }
    }
    validate_reserved_runtime_view(&cwd, &entry_program, &mount_plan, &masks).map_err(
        |reason| Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(reason),
        },
    )?;
    validate_entry_visibility(&entry_program, policy.fs.tmp, &masks, &mount_plan).map_err(
        |reason| Degradation {
            lost: vec!["process-entry".to_string()],
            reason: Some(reason),
        },
    )?;
    validate_cwd_visibility(&cwd, root_view, policy.fs.tmp, &masks, &mount_plan).map_err(
        |reason| Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some(reason),
        },
    )?;
    validate_dynamic_preload_view(runtime_image, &masks).map_err(|reason| Degradation {
        lost: vec!["runtime-closure".to_string()],
        reason: Some(reason),
    })?;
    let masks = masks
        .into_iter()
        .filter(|mask| !mask_already_enforced(mask))
        .collect::<Vec<_>>();
    // Open every executable candidate before any real proxy/tmp/CA support
    // resource exists. The admitted descriptor remains the launch authority. When
    // this launch must be able to nest, the ONLY admissible candidate is the
    // dedicated helper — never fall back to a stock candidate that cannot nest.
    let bwrap_candidates = open_bwrap_candidate_inventory(spec.require_nesting);
    if bwrap_candidates.candidates.is_empty() {
        let reason = classify_bwrap_failures(&bwrap_candidates.failures, spec.require_nesting);
        return Err(Degradation {
            lost: vec!["fs".to_string()],
            reason: Some(reason),
        });
    }
    let (bwrap, ca_placeholder) = admit_bwrap_candidate(
        policy,
        runtime,
        root_view,
        &entry_program,
        &mount_plan,
        &masks,
        bwrap_candidates,
        spec.require_nesting,
    )?;
    Ok(LinuxPreflight {
        landlock: None,
        confined: Some(ConfinedPreflight {
            root_view,
            cwd,
            entry_program,
            mount_plan,
            masks,
            bwrap,
            ca_placeholder,
        }),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<File>,
    tmp_dir: Option<&Path>,
    net_bridge_dir: Option<&Path>,
    runtime: Option<&super::linux_monitor::RuntimeCapability>,
    preflight: LinuxPreflight,
) -> Result<Prepared, Degradation> {
    if let Some(landlock) = preflight.landlock {
        return apply_landlock(policy, spec, landlock, tmp_dir);
    }
    let Some(confined) = preflight.confined else {
        return Ok(Prepared {
            command: base_command(&spec, policy),
            degradation: Degradation::full(),
            proxy: None,
            net_bridge: None,
            _inherited_files: Vec::new(),
            retained_monitor: None,
            signal_process_group: false,
            _private_tmp: None,
            redact_stdout: false,
            redact_stderr: false,
        });
    };

    let runtime = runtime.ok_or_else(|| Degradation {
        lost: vec!["runtime-capability-missing".to_string()],
        reason: Some(
            "Linux sandbox confinement requires the embedder's earliest bootstrap capability"
                .to_string(),
        ),
    })?;
    let _runtime = runtime;
    let ConfinedPreflight {
        root_view,
        cwd,
        entry_program,
        mount_plan,
        masks,
        bwrap,
        ca_placeholder,
    } = confined;
    // C3: per-host is ACTIVE iff the host bridge started (its socket dir is present to
    // bind-mount). The empty netns is the boundary either way; when per-host is active the
    // child reaches the proxy through the in-netns bridge on a FIXED loopback port (not the
    // parent's proxy port, which the netns cannot route to), and the seccomp socket ceiling
    // is dropped (the netns confines egress, so the child must be able to create sockets).
    let per_host = net_bridge_dir.is_some();
    let child_proxy_port = per_host.then_some(super::linux_monitor::IN_NETNS_PROXY_PORT);
    let target_env = target_environment(policy, child_proxy_port, proxy_token, ca_bundle.is_some());
    let bootstrap = super::linux_monitor::BootstrapSpec::new(
        runtime,
        policy,
        per_host,
        &spec,
        entry_program.as_os_str().to_owned(),
        cwd.clone(),
        target_env,
    )
    .map_err(|error| Degradation {
        lost: vec!["process-isolation".to_string()],
        reason: Some(format!("preparing retained monitor bootstrap: {error}")),
    })?;
    let retained_monitor = super::linux_monitor::RetainedMonitorLaunch::new(runtime, bootstrap)
        .map_err(|error| Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(format!("preparing retained monitor launch: {error}")),
        })?;

    let effective_ca = ca_bundle
        .or(ca_placeholder)
        .map(super::linux_monitor::RetainedMonitorLaunch::relocate_setup_file)
        .transpose()
        .map_err(|error| Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(format!(
                "relocating immutable CA-bundle descriptor: {error}"
            )),
        })?;
    // C3: open + relocate a descriptor to the host bridge socket dir so Bubblewrap can
    // `--ro-bind-fd` it read-only into the sandbox (fd-based, TOCTOU-checked). A directory
    // cannot be carried as bind DATA, so unlike the CA bundle this one stays on the fd form
    // — sound here because the dir is a real on-disk path Bubblewrap can resolve. The dir
    // itself stays alive on the returned `Prepared`'s `HostNetBridge`, so this descriptor is
    // valid through the launch.
    let net_bridge_fd = net_bridge_dir
        .map(|dir| {
            File::open(dir)
                .and_then(super::linux_monitor::RetainedMonitorLaunch::relocate_setup_file)
        })
        .transpose()
        .map_err(|error| Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(format!(
                "opening per-host egress bridge socket dir: {error}"
            )),
        })?;
    let RetainedOuterSetup {
        command,
        mut setup_files,
        degradation,
    } = configure_retained_outer(
        policy,
        root_view,
        &entry_program,
        &mount_plan,
        &masks,
        &bwrap,
        effective_ca.as_ref(),
        tmp_dir,
        proxy_port,
        net_bridge_fd.as_ref(),
        &retained_monitor,
    )?;
    setup_files.extend(effective_ca);
    setup_files.extend(net_bridge_fd);
    setup_files.push(bwrap.executable);

    Ok(Prepared {
        command,
        degradation,
        // The retained monitor is PID 1 of the child's namespace and reaps the tree itself.
        signal_process_group: false,
        proxy: None,
        net_bridge: None,
        _inherited_files: setup_files,
        retained_monitor: Some(retained_monitor),
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

struct RetainedOuterSetup {
    command: Command,
    setup_files: Vec<File>,
    degradation: Degradation,
}

#[allow(clippy::too_many_arguments)]
fn configure_retained_outer(
    policy: &SandboxPolicy,
    root_view: RootView,
    entry_program: &Path,
    mount_plan: &[MountGrant],
    masks: &[Mask],
    bwrap: &PinnedBubblewrapCandidate,
    ca_bundle: Option<&File>,
    tmp_dir: Option<&Path>,
    proxy_port: Option<u16>,
    net_bridge_fd: Option<&File>,
    retained_monitor: &super::linux_monitor::RetainedMonitorLaunch,
) -> Result<RetainedOuterSetup, Degradation> {
    let mut setup = Command::new("");
    let mut mask_sources = append_confinement_options(
        &mut setup,
        policy,
        root_view,
        entry_program,
        mount_plan,
        masks,
        tmp_dir,
        ca_bundle,
        net_bridge_fd,
        &|command| retained_monitor.append_runtime_mount(command),
    )?;

    let mut degradation = Degradation::full();
    // net-per-host degrades ONLY when a proxy was started but its host bridge did not
    // (`net_bridge_fd` absent) — then per-host collapses to coarse-deny (fail-SAFE:
    // denies more, not less). A wired bridge is full per-host enforcement.
    if policy.net.enforce && proxy_port.is_some() && net_bridge_fd.is_none() {
        degradation.lost.push("net-per-host".to_string());
        degradation.reason = Some(
            "per-host egress bridge could not be established; network was denied completely"
                .to_string(),
        );
    }

    retained_monitor
        .append_monitor_options(&mut setup)
        .map_err(|error| Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(format!("building retained monitor command: {error}")),
        })?;
    let arguments = write_bwrap_arguments(setup.get_args())
        .and_then(super::linux_monitor::RetainedMonitorLaunch::relocate_setup_file)
        .map_err(|error| Degradation {
            lost: vec!["process-entry".to_string()],
            reason: Some(format!("serializing Bubblewrap launch arguments: {error}")),
        })?;
    let mut command = Command::new(bwrap.program());
    command
        .env_clear()
        .arg("--args")
        .arg(arguments.as_raw_fd().to_string());
    retained_monitor.append_monitor_command(&mut command);
    let inherited_fds = mask_sources
        .iter()
        .chain(std::iter::once(&arguments))
        .map(AsRawFd::as_raw_fd)
        .chain(std::iter::once(bwrap.executable.as_raw_fd()))
        .chain(ca_bundle.into_iter().map(AsRawFd::as_raw_fd))
        .chain(net_bridge_fd.into_iter().map(AsRawFd::as_raw_fd))
        .collect::<Vec<_>>();
    retained_monitor
        .install_pre_exec(&mut command, &inherited_fds)
        .map_err(|error| Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(format!("sealing retained monitor descriptors: {error}")),
        })?;
    mask_sources.push(arguments);
    Ok(RetainedOuterSetup {
        command,
        setup_files: mask_sources,
        degradation,
    })
}

/// One filesystem operation the emitter writes, carrying which policy rule asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsOp<'a> {
    Bind(&'a MountGrant),
    Mask(&'a Mask),
}

/// Merge the compiled grants and the collected masks into the single stream the policy
/// authored, so last-match-wins survives into the mount layout.
///
/// One containment property is enforced here rather than assumed: a bind on a mask's own
/// path or on an ANCESTOR of it replaces that whole subtree with the host's view, so such
/// a mask is pushed past the bind instead of being layered under it. Last-match-wins
/// already keeps the shape from arising through the surface grammar — an ancestor allow
/// carries a `/**` twin that wins the mask's own path, so no mask is produced at all — but
/// a deny that survives collection must not depend on the compiler for that.
fn order_fs_operations<'a>(mount_plan: &'a [MountGrant], masks: &'a [Mask]) -> Vec<FsOp<'a>> {
    // Rank 0 vs 1 breaks a tie toward the bind, which is what puts a clamped mask AFTER
    // the bind it was clamped to rather than merely alongside it.
    let mut ops: Vec<(usize, u8, FsOp<'a>)> = mount_plan
        .iter()
        .map(|grant| (grant.rule_index, 0, FsOp::Bind(grant)))
        .collect();
    ops.extend(masks.iter().map(|mask| {
        let shadowed_by = mount_plan
            .iter()
            .filter(|grant| mask.path.starts_with(&grant.path))
            .map(|grant| grant.rule_index)
            .max();
        let order = shadowed_by.map_or(mask.order, |bind| mask.order.max(bind));
        (order, 1, FsOp::Mask(mask))
    }));
    ops.sort_by_key(|(order, rank, _)| (*order, *rank));
    ops.into_iter().map(|(_, _, op)| op).collect()
}

/// Whether any LATER operation binds something strictly inside `mask`, which the mask's
/// permissions must therefore leave traversable. A nested mask needs no traversal — it is
/// hidden either way — so only binds count.
fn reopened_below(rest: &[FsOp<'_>], mask: &Path) -> bool {
    rest.iter().any(|op| match op {
        FsOp::Bind(grant) => grant.path != mask && grant.path.starts_with(mask),
        FsOp::Mask(_) => false,
    })
}

/// Bubblewrap options that carry the whole policy, in the order Bubblewrap applies
/// them. Split out from [`configure_retained_outer`] so the option sequence — the
/// actual filesystem and namespace boundary — is assertable without a real
/// Bubblewrap, monitor image, or set of live descriptors; see the golden test
/// `confinement_options_pin_the_namespace_and_mount_boundary`. `runtime_mount` is
/// the monitor's private-runtime bind, which must land between the fresh `/dev`
/// and `/proc` views.
///
/// The returned files back every `--ro-bind-data` source and must outlive the spawn.
#[allow(clippy::too_many_arguments)]
fn append_confinement_options(
    setup: &mut Command,
    policy: &SandboxPolicy,
    root_view: RootView,
    entry_program: &Path,
    mount_plan: &[MountGrant],
    masks: &[Mask],
    tmp_dir: Option<&Path>,
    ca_bundle: Option<&File>,
    net_bridge_fd: Option<&File>,
    runtime_mount: &dyn Fn(&mut Command),
) -> Result<Vec<File>, Degradation> {
    // `--new-session` is the ONLY defence here against TIOCSTI terminal injection: a child
    // still holding the launcher's controlling tty can ioctl(TIOCSTI) bytes into the PARENT
    // SHELL's input queue, to be executed outside the sandbox. Measured with this exact flag
    // set minus that one flag, the injection lands — every other flag below leaves it open,
    // and so does seccomp (TIOCSTI is an ioctl request, not a filtered syscall).
    setup.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--as-pid-1",
        "--cap-drop",
        "ALL",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
    ]);

    match root_view {
        RootView::ReadWrite => {
            setup.args(["--bind", "/", "/"]);
        }
        RootView::ReadOnly => {
            setup.args(["--ro-bind", "/", "/"]);
        }
        RootView::Minimal => {
            append_minimal_read_mounts(setup, entry_program)?;
        }
    };
    setup.args(["--dev", "/dev"]);
    runtime_mount(setup);
    setup.args(["--proc", "/proc"]);

    match policy.fs.tmp {
        TmpMode::Shared => {}
        TmpMode::Private => {
            let Some(dir) = tmp_dir else {
                return Err(Degradation {
                    lost: vec!["tmp-private".to_string()],
                    reason: Some("private temporary directory could not be created".to_string()),
                });
            };
            setup.arg("--bind").arg(dir).arg("/tmp");
        }
        TmpMode::Deny => {
            setup.args(["--perms", "111", "--tmpfs", "/tmp"]);
        }
    }

    // ONE policy-ordered stream of binds and masks. Bubblewrap applies operations strictly
    // in argv order and CREATES its own mountpoints — `SETUP_BIND_MOUNT` calls
    // `ensure_dir`/`ensure_file`, whose parents come from `mkdir_with_parents` — so a bind
    // INSIDE an earlier deny's tmpfs layers on top of it with no `--dir` scaffolding. That
    // is the whole mechanism behind an interleaved policy like
    // `["./", "!./private", "./private/reopened"]`. Emitting every grant and only then
    // every mask, as this once did, throws the interleaving away: the mask lands last and
    // silently swallows the reopen, with the launch still reporting success.
    let ops = order_fs_operations(mount_plan, masks);
    let mut mask_sources = Vec::new();
    let mut deferred_remounts: Vec<&Path> = Vec::new();
    for position in 0..ops.len() {
        match ops[position] {
            FsOp::Bind(grant) => {
                setup
                    .arg(match grant.access {
                        // RESIDUAL: a bind is always a subtree, so bubblewrap cannot hold
                        // the node-only line Landlock can and renders `ListOnly` as the
                        // read-only subtree it used to be. The build jail runs on Landlock;
                        // this backend is the nesting/no-Landlock fallback, so the widening
                        // is bounded to that path rather than fixed by an empty `--dir`,
                        // which would HIDE contents a policy elsewhere granted.
                        MountAccess::ListOnly | MountAccess::ReadOnly => "--ro-bind",
                        MountAccess::ReadWrite => "--bind",
                    })
                    .arg(&grant.path)
                    .arg(&grant.path);
            }
            FsOp::Mask(mask) if !mask.directory => {
                let source = open_inheritable_dev_null()
                    .and_then(super::linux_monitor::RetainedMonitorLaunch::relocate_setup_file)
                    .map_err(|error| Degradation {
                        lost: vec!["fs-read-deny".to_string()],
                        reason: Some(format!("opening empty mask source: {error}")),
                    })?;
                setup
                    .arg("--perms")
                    .arg(match mask.kind {
                        MaskKind::EmptyReadable => "444",
                        MaskKind::Unreadable => "000",
                    })
                    .arg("--ro-bind-data")
                    .arg(source.as_raw_fd().to_string())
                    .arg(&mask.path);
                mask_sources.push(source);
            }
            FsOp::Mask(mask) => {
                // 000 removes TRAVERSAL, not merely listing, and every launch drops
                // CAP_DAC_READ_SEARCH — so a grant reopened underneath this mask would be
                // mounted and then unreachable. 111 is the only value that admits the
                // descent while still refusing to list the directory, read what it hides,
                // or accept a write; it is also what the Seatbelt backend expresses for
                // the same policy. A mask with nothing reopened below it keeps 000.
                let reopened = reopened_below(&ops[position + 1..], &mask.path);
                let perms = match mask.kind {
                    MaskKind::EmptyReadable => "555",
                    MaskKind::Unreadable if reopened => "111",
                    MaskKind::Unreadable => "000",
                };
                setup
                    .arg("--perms")
                    .arg(perms)
                    .arg("--tmpfs")
                    .arg(&mask.path);
                // `--remount-ro` is the write barrier that does not depend on the perms:
                // under nub's own `--cap-drop ALL` the mode already refuses the write, but a
                // caps-RETAINING flag set writes a perms-000 tmpfs happily, and 111 does not
                // refuse it at all. Applied immediately after its `--tmpfs`,
                // though, it seals the directory BEFORE Bubblewrap can create a nested
                // bind's mountpoint inside it, and the launch dies on EROFS from
                // `ensure_dir`. It does not recurse into submounts, so deferring every one
                // past the whole stream keeps the reopened binds writable and still seals
                // the mask itself.
                deferred_remounts.push(&mask.path);
            }
        }
    }
    for path in deferred_remounts {
        setup.arg("--remount-ro").arg(path);
    }
    // Nub infrastructure is layered after authored masks at a reserved destination
    // below the fresh /dev view. The child never receives the host temporary path,
    // so a Private/Deny /tmp or an ancestor mask cannot hide its trust bundle.
    // The bundle is an anonymous sealed memfd, which `--ro-bind-fd` CANNOT carry: Bubblewrap
    // turns that option into a bind on the literal source `/proc/self/fd/N` and `realpath()`s
    // it, and a memfd resolves to `/memfd:… (deleted)`. Bind DATA copies the bytes out instead
    // and never resolves a pathname. It reads from the descriptor's CURRENT offset and the
    // descriptor is a `dup` of a caller-owned file, so nothing here may assume offset 0. What
    // keeps two launches from interleaving on one shared offset is that `MitmCa` — hence the
    // memfd — is minted fresh per `apply`; the rewind alone would not make that safe.
    if let Some(bundle) = ca_bundle {
        Seek::rewind(&mut &*bundle).map_err(|error| Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(format!("rewinding the child CA-bundle descriptor: {error}")),
        })?;
        setup
            .arg("--perms")
            .arg("444")
            .arg("--ro-bind-data")
            .arg(bundle.as_raw_fd().to_string())
            .arg(super::linux_monitor::PRIVATE_CA_BUNDLE);
    }
    // C3: read-only bind-mount the host bridge socket dir into the sandbox. The child (and
    // the monitor's in-netns bridge) reach the loopback proxy ONLY through the UDS inside
    // it; the read-only mount keeps the untrusted child from unlinking or replacing the
    // socket (a connect is not a filesystem write, so the in-netns half still connects).
    if let Some(dir) = net_bridge_fd {
        setup
            .arg("--ro-bind-fd")
            .arg(dir.as_raw_fd().to_string())
            .arg(super::linux_net_bridge::PRIVATE_NET_ROOT);
    }
    setup
        .arg("--remount-ro")
        .arg(super::linux_monitor::PRIVATE_SUPPORT_ROOT);
    if policy.fs.tmp == TmpMode::Deny {
        setup.args(["--remount-ro", "/tmp"]);
    }
    if root_view == RootView::Minimal {
        setup.args(["--remount-ro", "/"]);
    }

    if policy.net.enforce {
        // The empty netns is THE egress boundary at EVERY posture: coarse-deny (no proxy),
        // per-host (egress reaches only the proxy via the bridge), and even the fallback
        // the caller records. C3 wires the per-host bridge here; it does NOT weaken
        // `--unshare-net`.
        setup.arg("--unshare-net");
    }

    Ok(mask_sources)
}

#[allow(clippy::too_many_arguments)]
fn admit_bwrap_candidate(
    policy: &SandboxPolicy,
    runtime: &super::linux_monitor::RuntimeCapability,
    root_view: RootView,
    entry_program: &Path,
    mount_plan: &[MountGrant],
    masks: &[Mask],
    inventory: PinnedCandidateInventory,
    require_nesting: bool,
) -> Result<(PinnedBubblewrapCandidate, Option<File>), Degradation> {
    // Candidate admission runs before the real per-launch proxy, CA, or private
    // temporary directory exists. These owned substitutes exercise the same
    // late-bound mount slots without exposing a real child resource to a rejected
    // executable candidate.
    let probe_tmp = if policy.fs.tmp == TmpMode::Private {
        Some(
            tempfile::Builder::new()
                .prefix("nub-bwrap-probe-tmp-")
                .tempdir()
                .map_err(|error| Degradation {
                    lost: vec!["tmp-private".to_string()],
                    reason: Some(format!(
                        "creating private temporary-directory probe slot: {error}"
                    )),
                })?,
        )
    } else {
        None
    };
    let probe_ca = if matches!(policy.net.inspection, crate::policy::Inspection::TlsInspect) {
        Some(
            sealed_support_file("nub-bwrap-probe-ca", &[])
                .and_then(super::linux_monitor::RetainedMonitorLaunch::relocate_setup_file)
                .map_err(|error| Degradation {
                    lost: vec!["net-per-host".to_string()],
                    reason: Some(format!("creating CA-bundle probe slot: {error}")),
                })?,
        )
    } else {
        None
    };

    let mut failures = inventory.failures;
    for candidate in inventory.candidates {
        let bootstrap = super::linux_monitor::BootstrapSpec::candidate_probe(
            runtime,
            policy.net.enforce,
            protects_ambient_credentials(policy),
            // A nesting launch relaxes the probe's inheritance-blind negative controls
            // (absent /proc/keys masks + a full-network target the inherited ceiling
            // may still deny) so it verifies the same view the real launch will have.
            require_nesting,
        )
        .map_err(|error| Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(format!("preparing Bubblewrap candidate probe: {error}")),
        })?;
        let retained_monitor = super::linux_monitor::RetainedMonitorLaunch::new(runtime, bootstrap)
            .map_err(|error| Degradation {
                lost: vec!["process-isolation".to_string()],
                reason: Some(format!("preparing Bubblewrap candidate monitor: {error}")),
            })?;
        let RetainedOuterSetup {
            mut command,
            setup_files,
            degradation: _,
        } = configure_retained_outer(
            policy,
            root_view,
            entry_program,
            mount_plan,
            masks,
            &candidate,
            probe_ca.as_ref(),
            probe_tmp.as_ref().map(tempfile::TempDir::path),
            None,
            None,
            &retained_monitor,
        )?;
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut outer = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                failures.push(format!(
                    "{}: candidate probe could not start: {error}",
                    candidate.source_path.display()
                ));
                continue;
            }
        };
        let mut stderr = outer.stderr.take();
        drop(setup_files);
        match retained_monitor.run_candidate_probe(outer, Duration::from_secs(5)) {
            Ok(()) => return Ok((candidate, probe_ca)),
            Err(error) => {
                let diagnostic_bytes = stderr
                    .as_mut()
                    .map(drain_probe_diagnostic)
                    .unwrap_or_default();
                let stderr = String::from_utf8_lossy(&diagnostic_bytes);
                let diagnostic = if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                };
                failures.push(format!(
                    "{}: candidate probe failed: {error}{diagnostic}",
                    candidate.source_path.display(),
                ));
            }
        }
    }
    let reason = classify_bwrap_failures(&failures, require_nesting);
    Err(Degradation {
        lost: vec!["fs".to_string()],
        reason: Some(reason),
    })
}

const PROBE_DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const PROBE_DIAGNOSTIC_DEADLINE: Duration = Duration::from_millis(250);

/// Collect a failed candidate probe's stderr for the failure message.
///
/// The read is bounded on both axes because the probe's stderr write end is
/// inheritable: a descendant that outlives the probe keeps the pipe open, so
/// waiting for EOF would block `apply()` for as long as that descendant lives.
/// The payload is a diagnostic fragment only, so truncating it is always
/// preferable to stalling sandbox setup.
fn drain_probe_diagnostic(stderr: &mut ChildStderr) -> Vec<u8> {
    // SAFETY: the descriptor is owned by `stderr` and stays open for the call.
    let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Vec::new();
    }
    let deadline = Instant::now() + PROBE_DIAGNOSTIC_DEADLINE;
    let mut collected = Vec::new();
    let mut chunk = [0u8; 4096];
    while collected.len() < PROBE_DIAGNOSTIC_LIMIT {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let room = PROBE_DIAGNOSTIC_LIMIT - collected.len();
                collected.extend_from_slice(&chunk[..read.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    collected
}

fn target_environment(
    policy: &SandboxPolicy,
    // The loopback port the CHILD is pointed at — on Linux per-host this is the FIXED
    // in-netns bridge port (the empty netns cannot route to the parent proxy port), and
    // `None` for coarse-deny/full (no cooperative proxy env is set).
    child_proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: bool,
) -> BTreeMap<OsString, OsString> {
    let mut target_env = policy
        .env
        .constructed
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<BTreeMap<_, _>>();
    if let Some(port) = child_proxy_port {
        super::insert_proxy_env(&mut target_env, port, proxy_token);
    }
    if ca_bundle {
        super::insert_ca_env(
            &mut target_env,
            Path::new(super::linux_monitor::PRIVATE_CA_BUNDLE),
        );
    }
    if policy.fs.tmp == TmpMode::Private {
        super::insert_tmp_env(&mut target_env, Path::new("/tmp"));
    }
    target_env
}

fn root_view(policy: &SandboxPolicy) -> RootView {
    if !fs_confines(&policy.fs) {
        return RootView::ReadWrite;
    }
    let mut decision = policy.fs.rules.default_effect;
    let mut access = FsAccess::Read;
    for rule in &policy.fs.rules.entries {
        if linux_grants::is_whole_root(rule.matcher.as_str()) {
            decision = rule.effect;
            access = rule.access;
        }
    }
    if decision == Effect::Allow {
        debug_assert!(matches!(access, FsAccess::Read | FsAccess::ReadWrite));
        RootView::ReadOnly
    } else {
        RootView::Minimal
    }
}

fn validate_reserved_runtime_view(
    cwd: &Path,
    entry_program: &Path,
    mount_plan: &[MountGrant],
    masks: &[Mask],
) -> Result<(), String> {
    let reserved = Path::new("/dev/.nub-sandbox");
    for (label, path) in [("working directory", cwd), ("entry program", entry_program)] {
        if path == reserved || path.starts_with(reserved) {
            return Err(format!(
                "sandbox {label} overlaps the reserved monitor runtime at {}",
                super::linux_monitor::PRIVATE_RUNTIME_ROOT
            ));
        }
    }
    for grant in mount_plan {
        if paths_overlap(&grant.path, reserved) {
            return Err(format!(
                "filesystem grant {} overlaps the reserved monitor runtime at {}",
                grant.path.display(),
                super::linux_monitor::PRIVATE_RUNTIME_ROOT
            ));
        }
    }
    for mask in masks {
        if paths_overlap(&mask.path, reserved) {
            return Err(format!(
                "filesystem deny {} overlaps the reserved monitor runtime at {}",
                mask.path.display(),
                super::linux_monitor::PRIVATE_RUNTIME_ROOT
            ));
        }
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn validate_dynamic_preload_view(
    runtime: &super::linux_monitor::RuntimeImage,
    masks: &[Mask],
) -> Result<(), String> {
    if !matches!(
        &runtime.kind,
        super::linux_monitor::RuntimeKind::Dynamic {
            family: super::linux_monitor::LoaderFamily::Glibc,
            ..
        }
    ) {
        return Ok(());
    }
    let logical = Path::new("/etc/ld.so.preload");
    let metadata = match fs::symlink_metadata(logical) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "inspecting the dynamic-loader preload file: {error}"
            ));
        }
    };
    let resolved = fs::canonicalize(logical).unwrap_or_else(|_| logical.to_path_buf());
    let hidden = masks.iter().any(|mask| {
        mask.path == logical
            || mask.path == resolved
            || (mask.directory
                && (logical.starts_with(&mask.path) || resolved.starts_with(&mask.path)))
    });
    if hidden {
        return Ok(());
    }
    Err(format!(
        "dynamic monitor startup refuses the visible {} at /etc/ld.so.preload; deny that path so the final sandbox view masks it",
        if metadata.file_type().is_symlink() {
            "symlink"
        } else {
            "file"
        }
    ))
}

fn append_minimal_read_mounts(
    command: &mut Command,
    entry_program: &Path,
) -> Result<(), Degradation> {
    let mut mounted = BTreeSet::new();
    for path in ESSENTIAL_READ_PATHS {
        append_ro_mount(command, Path::new(path), &mut mounted);
    }
    // The entry program is bound in its OWN right, so an interpreter outside the floor
    // (nub's provisioned Node under its store, a runner's `/opt/hostedtoolcache` Node)
    // stays launchable no matter how narrow the floor gets.
    append_ro_mount(command, entry_program, &mut mounted);
    Ok(())
}

/// Bind one floor path read-only, skipping what the host does not have. Absence-tolerance
/// is what lets the floor enumerate paths that are distro- and layout-specific
/// (`/etc/ld.so.preload` and `/libx32` exist almost nowhere); binding a missing source
/// would otherwise abort every confined run. bwrap creates the destination's parent dirs,
/// so a FILE entry under an unmounted `/etc` still lands.
fn append_ro_mount(command: &mut Command, path: &Path, mounted: &mut BTreeSet<PathBuf>) {
    if path.exists() && mounted.insert(path.to_path_buf()) {
        command.arg("--ro-bind").arg(path).arg(path);
    }
}

fn collect_masks(
    policy: &SandboxPolicy,
    deny_search_roots: &[PathBuf],
) -> Result<Vec<Mask>, String> {
    let matcher = PathMatcher::new(&policy.fs.rules);
    let mut candidates: Vec<(PathBuf, PathBuf, MaskKind, bool)> = Vec::new();
    let mut needs_bounded_snapshot = false;

    for rule in &policy.fs.rules.entries {
        if rule.effect != Effect::Deny {
            continue;
        }
        let pattern = rule.matcher.as_str();
        // Builtin secret-file floor globs (`.env*`/`.npmrc`, rootless + `**/` twins) are
        // ALWAYS enforced via the recursive snapshot — never as an exact path. The rootless
        // twins have no anchor and no glob metachar (`.npmrc`), so `exact_rule_root` would
        // otherwise resolve them against the HOST cwd; the `**/` twin is what masks every
        // match under the deny-search roots at any depth.
        if is_builtin_env_glob(pattern) {
            needs_bounded_snapshot = true;
            continue;
        }
        if let Some(path) = exact_rule_root(pattern) {
            candidates.push((
                path.clone(),
                path,
                MaskKind::Unreadable,
                pattern.ends_with("/**"),
            ));
            continue;
        }
        if is_direct_snapshot_glob(pattern, deny_search_roots) {
            needs_bounded_snapshot = true;
        } else {
            return Err(format!(
                "deny glob {pattern:?} cannot be enforced by a bounded startup snapshot"
            ));
        }
    }

    if needs_bounded_snapshot {
        collect_direct_denied_candidates(policy, deny_search_roots, &matcher, &mut candidates)?;
    }

    let mut masks = Vec::new();
    for (logical, path, kind, masks_subtree) in candidates {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "statting deny candidate {}: {error}",
                    path.display()
                ));
            }
        };
        let path = fs::canonicalize(&path)
            .map_err(|e| format!("resolving deny candidate {}: {e}", path.display()))?;
        let verdict = matcher.decide_logical_or_resolved(&logical, &path);
        if masks_subtree && verdict.effect == Effect::Allow {
            return Err(format!(
                "denied subtree {} has an exact directory allow that stock Bubblewrap cannot preserve",
                path.display()
            ));
        }
        if verdict.effect == Effect::Deny || masks_subtree {
            reject_denied_hardlink(&logical, &path, &metadata)?;
            // The mask belongs where the policy LAST spoke about this path — the same
            // evaluation `verdict` just made, read as a position instead of an effect.
            // Deriving it here rather than from the loop above covers the snapshot-walk
            // candidates identically: they have no single originating rule, and the deny
            // that decided them is exactly the one whose position they should take.
            let order = matcher
                .last_matching_index(&logical, &path)
                .unwrap_or(INFRASTRUCTURE_ORDER);
            masks.push(Mask {
                path,
                kind,
                directory: metadata.is_dir(),
                order,
            });
        }
    }
    Ok(merge_masks(masks))
}

fn reject_denied_hardlink(
    logical: &Path,
    resolved: &Path,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    if !metadata.is_file() || metadata.nlink() <= 1 {
        return Ok(());
    }
    let aliases = metadata.nlink();
    if logical == resolved {
        Err(format!(
            "denied regular file {} has {aliases} hard links; stock Bubblewrap cannot hide its aliases",
            logical.display()
        ))
    } else {
        Err(format!(
            "denied regular file {} resolves to {} with {aliases} hard links; stock Bubblewrap cannot hide its aliases",
            logical.display(),
            resolved.display()
        ))
    }
}

/// Directory names never descended during the recursive deny snapshot. `node_modules`
/// is skipped for COST — a monorepo's tree is enormous and this snapshot runs per
/// lifecycle spawn — and correctness does not need it: a dep-internal `.env`/`.npmrc`
/// there is the dependency's OWN shipped file, not a USER secret, and the confined dep
/// runs as the same uid so it can read its own files regardless. The USER's secrets live
/// in project SOURCE (`apps/*/.env`, `packages/*/.env`, a project `.npmrc` at any depth),
/// which the walk covers fully. `.git` is skipped for cost with no secret-file matches to
/// lose. (Single-`*` user globs still bind only at their own depth — the matcher, not the
/// walk, decides each candidate — so the recursion never over-masks.)
const DENY_WALK_SKIP_DIRS: &[&str] = &["node_modules", ".git"];

fn collect_direct_denied_candidates(
    policy: &SandboxPolicy,
    roots: &[PathBuf],
    matcher: &PathMatcher,
    out: &mut Vec<(PathBuf, PathBuf, MaskKind, bool)>,
) -> Result<(), String> {
    let roots = strict_search_roots(roots)?;
    // The boundary between an explicit USER `.env` deny (band 1) and the builtin floor,
    // which decides the dotenv mask kind. Shared with the compiler so the recognizer reads
    // the same arrays `finalize_env_deny` emits rather than restating them — a restated
    // copy desyncs silently, and "floor not found" downgrades a mask instead of failing.
    let band_start = crate::compiler::env_deny_floor_start(&policy.fs.rules.entries);
    for root in roots {
        walk_deny_candidates(&root, matcher, band_start, out)?;
    }
    Ok(())
}

/// Recursively enumerate `dir`, masking every existing file the deny rules block at ANY
/// depth. The project subtree is bind-mounted read-only as ONE tree, so a NESTED secret
/// file (`apps/web/.env`, `packages/api/.npmrc`) would otherwise be readable in-jail even
/// though the `**/.env*`/`**/.npmrc` floor denies it — this walk is what makes stock
/// Bubblewrap enforce the depth-independent deny. Directory SYMLINKS are never followed
/// (no subtree escape, no cycle); a symlinked secret FILE is still masked via its resolved
/// target. A directory that ITSELF matches a deny rule (a `.env*`-named dir) is masked as a
/// whole and NOT descended. A subdir the host cannot enumerate is skipped, not fatal: the
/// confined child runs as the same uid, so a dir the host can't read the child can't either.
fn walk_deny_candidates(
    dir: &Path,
    matcher: &PathMatcher,
    band_start: Option<usize>,
    out: &mut Vec<(PathBuf, PathBuf, MaskKind, bool)>,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(e) => {
            return Err(format!(
                "enumerating deny-search dir {}: {e}",
                dir.display()
            ));
        }
    };
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("enumerating deny-search dir {}: {e}", dir.display()))?;
        let name = entry.file_name();
        let logical = dir.join(&name);
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "statting deny candidate {}: {error}",
                    logical.display()
                ));
            }
        };
        // Mask the entry itself if a deny rule matches it (a `.env*`/`.npmrc` FILE, or a
        // `.env*`-named DIRECTORY masked as a whole). Only a REAL, non-matching directory
        // is descended — a directory symlink (`file_type.is_dir() == false`) is treated as
        // a leaf candidate, never followed.
        let masked = consider_deny_candidate(&logical, &name, matcher, band_start, out)?;
        if file_type.is_dir()
            && !masked
            && !name
                .to_str()
                .is_some_and(|n| DENY_WALK_SKIP_DIRS.contains(&n))
        {
            walk_deny_candidates(&logical, matcher, band_start, out)?;
        }
    }
    Ok(())
}

/// Decide whether `logical` is masked by a deny rule and, if so, push its mask candidate.
/// Returns whether a mask was pushed (so the caller skips descending a masked directory).
/// The `.env*` dotenv basename maps to `EmptyReadable` (present-but-empty, so a dotenv
/// reader sees no secret rather than a hard error) UNLESS an explicit USER deny (in band 1,
/// before the builtin floor at `band_start`) upgrades it to `Unreadable`; every other
/// denied file — including `.npmrc` — is `Unreadable`.
fn consider_deny_candidate(
    logical: &Path,
    name: &OsStr,
    matcher: &PathMatcher,
    band_start: Option<usize>,
    out: &mut Vec<(PathBuf, PathBuf, MaskKind, bool)>,
) -> Result<bool, String> {
    match fs::metadata(logical) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "statting deny candidate {}: {error}",
                logical.display()
            ));
        }
    }
    let resolved = fs::canonicalize(logical)
        .map_err(|e| format!("resolving deny candidate {}: {e}", logical.display()))?;
    if !matcher.matches_deny_entry(logical, &resolved) {
        return Ok(false);
    }
    if matcher
        .decide_logical_or_resolved(logical, &resolved)
        .effect
        != Effect::Deny
    {
        return Ok(false);
    }
    let dotenv_name = name.to_str().is_some_and(|name| name.starts_with(".env"));
    let explicit_user_deny = band_start.is_some_and(|end| {
        matcher.last_matching_effect_before(logical, &resolved, end) == Some(Effect::Deny)
    });
    let kind = if dotenv_name && !explicit_user_deny {
        MaskKind::EmptyReadable
    } else {
        MaskKind::Unreadable
    };
    out.push((logical.to_path_buf(), resolved, kind, false));
    Ok(true)
}

fn strict_search_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut roots_out = BTreeSet::new();
    for root in roots {
        let root = fs::canonicalize(root)
            .map_err(|e| format!("resolving deny-search root {}: {e}", root.display()))?;
        let metadata = fs::metadata(&root)
            .map_err(|e| format!("statting deny-search root {}: {e}", root.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "deny-search root is not a directory: {}",
                root.display()
            ));
        }
        roots_out.insert(root);
    }
    Ok(roots_out.into_iter().collect())
}

fn merge_masks(masks: Vec<Mask>) -> Vec<Mask> {
    let mut merged: BTreeMap<PathBuf, Mask> = BTreeMap::new();
    for mask in masks {
        merged
            .entry(mask.path.clone())
            .and_modify(|current| {
                if mask.kind == MaskKind::Unreadable {
                    current.kind = MaskKind::Unreadable;
                }
                current.directory |= mask.directory;
                // Two masks on one path merge to the LATER position, so a path that is
                // both policy-denied and backend infrastructure keeps the infrastructure
                // mask's unconditional placement instead of sorting under a bind.
                current.order = current.order.max(mask.order);
            })
            .or_insert(mask);
    }
    let directories = merged
        .values()
        .filter(|mask| mask.directory)
        .map(|mask| mask.path.clone())
        .collect::<Vec<_>>();
    merged
        .into_values()
        .filter(|mask| {
            mask.directory
                || !directories
                    .iter()
                    .any(|dir| mask.path != *dir && mask.path.starts_with(dir))
        })
        .collect()
}

fn validate_entry_visibility(
    entry: &Path,
    tmp: TmpMode,
    masks: &[Mask],
    grants: &[linux_grants::MountGrant],
) -> Result<(), String> {
    if masks
        .iter()
        .any(|mask| entry == mask.path || (mask.directory && entry.starts_with(&mask.path)))
    {
        return Err(format!(
            "sandbox entry program is hidden by the final filesystem policy: {}",
            entry.display()
        ));
    }
    if tmp != TmpMode::Shared
        && entry.starts_with("/tmp")
        && !grants
            .iter()
            .any(|grant| entry == grant.path || entry.starts_with(&grant.path))
    {
        return Err(format!(
            "sandbox entry program is hidden by the temporary-directory policy: {}",
            entry.display()
        ));
    }
    Ok(())
}

fn validate_cwd_visibility(
    cwd: &Path,
    root_view: RootView,
    tmp: TmpMode,
    masks: &[Mask],
    grants: &[linux_grants::MountGrant],
) -> Result<(), String> {
    if masks
        .iter()
        .any(|mask| cwd == mask.path || (mask.directory && cwd.starts_with(&mask.path)))
    {
        return Err(format!(
            "sandbox working directory is hidden by the final filesystem policy: {}",
            cwd.display()
        ));
    }
    let granted = grants.iter().any(|grant| {
        cwd == grant.path || cwd.starts_with(&grant.path) || grant.path.starts_with(cwd)
    });
    if tmp != TmpMode::Shared && cwd.starts_with("/tmp") && !granted {
        return Err(format!(
            "sandbox working directory is hidden by the temporary-directory policy: {}",
            cwd.display()
        ));
    }
    if root_view == RootView::Minimal
        && !granted
        && !ESSENTIAL_READ_PATHS
            .iter()
            .any(|root| cwd == Path::new(root) || cwd.starts_with(root))
    {
        return Err(format!(
            "sandbox working directory is absent from the final filesystem view: {}",
            cwd.display()
        ));
    }
    Ok(())
}

/// Filesystem-path AF_UNIX sockets whose reach is NETWORK-EQUIVALENT: a child that can
/// connect to one has egress THROUGH the empty-netns wall, because the daemon or bus on the
/// far end can itself reach the network or the host (the container runtimes double as a full
/// host escape). The empty netns does not confine a PATH socket — that channel is scoped to
/// the MOUNT namespace, not the net namespace — so net confinement must close it at the fs
/// layer. Under `net.enforce` these are masked unreadable, so per-host and coarse-deny are
/// SELF-CONTAINED (they do not silently depend on the user's fs policy hiding these paths).
///
/// This does NOT touch ABSTRACT-namespace AF_UNIX (scoped to the empty net namespace, so it
/// can only reach in-sandbox peers) nor the in-netns relay's own UDS (a nub-private
/// read-only mount, not in this set) — so legit local IPC and the bridge keep working. Only
/// host-existing sockets are masked (an absent path is unreachable anyway); a regular file
/// or dir sharing the name is skipped (only a real socket is the network-equivalent hazard).
fn net_equivalent_socket_masks() -> Result<Vec<Mask>, String> {
    let mut paths: Vec<PathBuf> = [
        "/run/docker.sock",
        "/var/run/docker.sock",
        "/run/podman/podman.sock",
        "/var/run/podman/podman.sock",
        "/run/containerd/containerd.sock",
        "/var/run/containerd/containerd.sock",
        "/run/dbus/system_bus_socket",
        "/var/run/dbus/system_bus_socket",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Per-uid runtime sockets under XDG_RUNTIME_DIR: the D-Bus SESSION bus (its own
    // network-reachable services — portals, etc.) AND the ROOTLESS container-runtime API
    // sockets, which are the DEFAULT for rootless Docker/Podman and are just as much a full
    // host + off-box escape as their rootful counterparts.
    let uid = unsafe { libc::getuid() };
    for name in ["bus", "docker.sock", "podman/podman.sock"] {
        paths.push(PathBuf::from(format!("/run/user/{uid}/{name}")));
    }

    let mut masks = Vec::new();
    for path in paths {
        let Some(mask) = net_equivalent_socket_mask(&path)? else {
            continue;
        };
        masks.push(mask);
    }
    Ok(masks)
}

fn net_equivalent_socket_mask(path: &Path) -> Result<Option<Mask>, String> {
    let link = match fs::symlink_metadata(path) {
        Ok(link) => link,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            // The socket is already unreachable through a locked parent, but
            // those permissions can change after preflight. Mask the nearest
            // directory we can name so the sandbox remains closed if they do.
            return inaccessible_socket_parent_mask(path).map(Some);
        }
        Err(error) => {
            return Err(format!(
                "statting network-equivalent socket {}: {error}",
                path.display()
            ));
        }
    };
    let resolved = match fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return inaccessible_socket_parent_mask(path).map(Some);
        }
        Err(error) => {
            return Err(format!(
                "resolving network-equivalent socket {}: {error}",
                path.display()
            ));
        }
    };
    let is_socket = link.file_type().is_socket()
        || fs::metadata(&resolved).is_ok_and(|meta| meta.file_type().is_socket());
    if !is_socket {
        return Ok(None);
    }
    Ok(Some(Mask {
        path: resolved,
        kind: MaskKind::Unreadable,
        directory: false,
        order: INFRASTRUCTURE_ORDER,
    }))
}

fn inaccessible_socket_parent_mask(path: &Path) -> Result<Mask, String> {
    for parent in path.ancestors().skip(1) {
        // Hiding all of /run or /var would break the process view. A normal
        // locked runtime socket always has a narrower visible parent.
        if matches!(parent.to_str(), Some("/" | "/run" | "/var" | "/var/run")) {
            break;
        }
        match fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => {
                let resolved = fs::canonicalize(parent).map_err(|error| {
                    format!(
                        "resolving inaccessible network-equivalent socket parent {}: {error}",
                        parent.display()
                    )
                })?;
                return Ok(Mask {
                    path: resolved,
                    kind: MaskKind::Unreadable,
                    directory: true,
                    order: INFRASTRUCTURE_ORDER,
                });
            }
            Ok(_) => {
                return Err(format!(
                    "network-equivalent socket parent is not a directory: {}",
                    parent.display()
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "statting network-equivalent socket parent {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    Err(format!(
        "network-equivalent socket {} is inaccessible and has no narrow parent that can be masked",
        path.display()
    ))
}

fn alternate_procfs_masks() -> Result<Vec<Mask>, String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("reading process mount table: {e}"))?;
    let mut masks = Vec::new();
    for line in mountinfo.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            return Err("process mount table contained an invalid record".to_string());
        };
        if fields.get(separator + 1) != Some(&"proc") {
            continue;
        }
        let Some(encoded_mountpoint) = fields.get(4) else {
            return Err("process mount table omitted a mount point".to_string());
        };
        let mountpoint = PathBuf::from(unescape_mountinfo_path(encoded_mountpoint)?);
        if mountpoint == Path::new("/proc") || mountpoint.starts_with("/proc/") {
            continue;
        }
        if mountpoint.exists() {
            masks.push(Mask {
                path: mountpoint,
                kind: MaskKind::Unreadable,
                directory: true,
                order: INFRASTRUCTURE_ORDER,
            });
        }
    }
    Ok(masks)
}

fn keyring_procfs_masks() -> Result<Vec<Mask>, String> {
    ["/proc/keys", "/proc/key-users"]
        .into_iter()
        .map(|path| {
            let path = PathBuf::from(path);
            let metadata = fs::metadata(&path).map_err(|error| {
                format!("statting keyring procfs entry {}: {error}", path.display())
            })?;
            if !metadata.is_file() {
                return Err(format!(
                    "keyring procfs entry is not a regular file: {}",
                    path.display()
                ));
            }
            Ok(Mask {
                path,
                kind: MaskKind::Unreadable,
                directory: false,
                order: INFRASTRUCTURE_ORDER,
            })
        })
        .collect()
}

pub(super) fn protects_ambient_credentials(policy: &SandboxPolicy) -> bool {
    policy.env.resolved && policy.env.enforce && !policy.env.withheld.is_empty()
}

fn unescape_mountinfo_path(encoded: &str) -> Result<OsString, String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let Some(octal) = bytes.get(index + 1..index + 4) else {
            return Err("process mount table contained a truncated escape".to_string());
        };
        if !octal.iter().all(u8::is_ascii_digit) || octal.iter().any(|digit| *digit > b'7') {
            return Err("process mount table contained an invalid escape".to_string());
        }
        decoded.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0'));
        index += 4;
    }
    Ok(OsString::from_vec(decoded))
}

/// Membership in the builtin secret-file floor (the `.env*` bands plus the npm-config
/// leaf twins). Reads the `ENV_DENY_*_GLOBS` arrays directly so it cannot desync from
/// what `fold::finalize_env_deny` actually emits.
fn is_builtin_env_glob(pattern: &str) -> bool {
    crate::compiler::ENV_DENY_LEAF_GLOBS
        .iter()
        .chain(crate::compiler::ENV_DENY_SUBTREE_GLOBS)
        .any(|glob| *glob == pattern)
}

fn is_direct_snapshot_glob(pattern: &str, roots: &[PathBuf]) -> bool {
    if is_builtin_env_glob(pattern) {
        return true;
    }
    roots.iter().any(|root| {
        let root = canonicalize_including_nonexistent(root);
        let Some(root) = root.to_str() else {
            return false;
        };
        let Some(relative) = pattern
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return false;
        };
        let trimmed = relative.strip_suffix("/**").unwrap_or(relative);
        if is_builtin_env_glob(relative) || is_builtin_env_glob(trimmed) {
            return true;
        }
        if !trimmed.contains('/') {
            return has_glob_meta(trimmed);
        }
        trimmed
            .strip_prefix("**/")
            .is_some_and(|leaf| !leaf.is_empty() && !leaf.contains('/'))
    })
}

fn has_glob_meta(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '{'])
}

fn mask_already_enforced(mask: &Mask) -> bool {
    let Ok(metadata) = fs::metadata(&mask.path) else {
        return false;
    };
    let Some(parent) = mask.path.parent() else {
        return false;
    };
    let Ok(parent_metadata) = fs::metadata(parent) else {
        return false;
    };
    // Nub's synthetic file/directory is a distinct read-only tmpfs mount. Requiring
    // both properties avoids trusting an ordinary empty file that the child could
    // replace. An outer Nub sandbox's mount is pinned and already at least as strict.
    if metadata.dev() == parent_metadata.dev() || !mount_is_read_only(&mask.path) {
        return false;
    }
    let inherited_kind = if metadata.is_dir() {
        if fs::read_dir(&mask.path).is_err() {
            Some(MaskKind::Unreadable)
        } else if fs::read_dir(&mask.path).is_ok_and(|mut entries| entries.next().is_none()) {
            Some(MaskKind::EmptyReadable)
        } else {
            None
        }
    } else if metadata.permissions().mode() & 0o444 == 0 {
        Some(MaskKind::Unreadable)
    } else if metadata.len() == 0 {
        Some(MaskKind::EmptyReadable)
    } else {
        None
    };
    matches!(
        (inherited_kind, mask.kind),
        (Some(MaskKind::Unreadable), _) | (Some(MaskKind::EmptyReadable), MaskKind::EmptyReadable)
    )
}

fn mount_is_read_only(path: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return false;
    }
    let stat = unsafe { stat.assume_init() };
    stat.f_flag & libc::ST_RDONLY != 0
}

fn cap_inherited_read_only(plan: &mut [linux_grants::MountGrant]) {
    cap_inherited_read_only_with(plan, mount_is_read_only);
}

fn cap_inherited_read_only_with(
    plan: &mut [linux_grants::MountGrant],
    is_read_only: impl Fn(&Path) -> bool,
) {
    for grant in plan {
        if grant.access == MountAccess::ReadWrite && is_read_only(&grant.path) {
            grant.access = MountAccess::ReadOnly;
        }
    }
}

fn exact_rule_root(pattern: &str) -> Option<PathBuf> {
    let trimmed = pattern.strip_suffix("/**").unwrap_or(pattern);
    if trimmed.contains(['*', '?', '[', '{']) || trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn validate_process_inputs(spec: &CommandSpec) -> Result<(), String> {
    let reject_nul = |label: &str, value: &OsStr| {
        if value.as_bytes().contains(&0) {
            Err(format!("sandbox {label} contains a NUL byte"))
        } else {
            Ok(())
        }
    };
    reject_nul("entry program", &spec.program)?;
    for (index, arg) in spec.args.tokens().enumerate() {
        reject_nul(&format!("argument {index}"), arg)?;
    }
    if let Some(cwd) = &spec.cwd {
        reject_nul("working directory", cwd.as_os_str())?;
    }
    Ok(())
}

struct PinnedCandidateInventory {
    candidates: Vec<PinnedBubblewrapCandidate>,
    failures: Vec<String>,
}

fn open_bwrap_candidate_inventory(require_nesting: bool) -> PinnedCandidateInventory {
    // A nesting launch admits ONLY the dedicated helper: on an AppArmor-restricted
    // host a stock outer cannot transition to the helper's profile, so falling back
    // to a system/bundled candidate would silently yield an un-nestable sandbox.
    if require_nesting {
        return open_bwrap_candidate_inventory_from([PathBuf::from(DEDICATED_HELPER_PATH)], [], []);
    }
    let (dedicated, system, bundled) = single_level_bwrap_candidate_paths();
    open_bwrap_candidate_inventory_from(dedicated, system, bundled)
}

/// The single-level candidate paths, in resolution order: dedicated helper, then the
/// stock system paths, then the bundled resource beside the running binary.
///
/// B2: single-level prefers the fixed-path helper too. On an AppArmor-restricted host (24.04)
/// it is the ONLY candidate that can create the userns — a stock system/bundled bwrap at an
/// unprofiled path is denied. When the helper is absent (no setup, or an unrestricted host
/// like 22.04 / sysctl=0) its open fails and the resolver falls through to system/bundled, so
/// the no-setup path is unchanged where setup isn't needed. `nub setup-sandbox` installs it.
///
/// Split out so [`crate::backend::linux_probe`] can ask the SAME question the resolver
/// asks; a probe that enumerates its own paths drifts out of step with production and
/// then reports a host as unable to enforce when only its hardcoded pair was denied.
pub(crate) fn single_level_bwrap_candidate_paths() -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let dedicated = vec![PathBuf::from(DEDICATED_HELPER_PATH)];
    let system = vec![PathBuf::from("/usr/bin/bwrap"), PathBuf::from("/bin/bwrap")];
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .map(|directory| {
            vec![
                directory.join("nub-resources/bwrap"),
                directory.join("../nub-resources/bwrap"),
            ]
        })
        .unwrap_or_default();
    (dedicated, system, bundled)
}

/// Validate the Linux resource paired with the running binary before a staged
/// install is published. The digest pins the exact upstream build; the version
/// is compiled alongside it so release assembly cannot accidentally pair a
/// resource with a binary built for a different Bubblewrap release.
pub fn validate_adjacent_resource_bundle() -> Result<(), String> {
    let expected_version = option_env!("NUB_BWRAP_VERSION")
        .ok_or_else(|| "Nub was built without a bundled Bubblewrap version".to_string())?;
    if expected_version != "0.11.2" {
        return Err(format!(
            "Nub was built for unsupported bundled Bubblewrap {expected_version}"
        ));
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("locating the staged Nub executable: {error}"))?;
    let resource = executable
        .parent()
        .ok_or_else(|| "the staged Nub executable has no parent directory".to_string())?
        .join("nub-resources/bwrap");
    open_pinned_bwrap_candidate(&resource, BubblewrapOrigin::Bundled).map(|_| ())
}

fn open_bwrap_candidate_inventory_from(
    dedicated: impl IntoIterator<Item = PathBuf>,
    system: impl IntoIterator<Item = PathBuf>,
    bundled: impl IntoIterator<Item = PathBuf>,
) -> PinnedCandidateInventory {
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    // The dedicated helper is preferred ahead of every stock candidate: it is the
    // only origin that can launch the outermost sandbox of a nesting chain.
    for (origin, paths) in [
        (
            BubblewrapOrigin::DedicatedHelper,
            dedicated.into_iter().collect::<Vec<_>>(),
        ),
        (
            BubblewrapOrigin::System,
            system.into_iter().collect::<Vec<_>>(),
        ),
        (
            BubblewrapOrigin::Bundled,
            bundled.into_iter().collect::<Vec<_>>(),
        ),
    ] {
        for path in paths {
            match open_pinned_bwrap_candidate(&path, origin) {
                Ok(candidate) => {
                    if seen.insert(candidate.source_identity) {
                        candidates.push(candidate);
                    }
                }
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
    }
    PinnedCandidateInventory {
        candidates,
        failures,
    }
}

fn open_pinned_bwrap_candidate(
    path: &Path,
    origin: BubblewrapOrigin,
) -> Result<PinnedBubblewrapCandidate, String> {
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "candidate path contains a NUL byte".to_string())?;
    let fd = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        // For the dedicated helper an EACCES open is the group-access signal:
        // the 0750 root:nub-sandbox helper is unopenable by a non-member. Tag it
        // so the nesting diagnostic can name the missing group access precisely.
        if origin == BubblewrapOrigin::DedicatedHelper && error.raw_os_error() == Some(libc::EACCES)
        {
            return Err(format!("{DEDICATED_HELPER_ACCESS_TAG}: {error}"));
        }
        return Err(format!("opening candidate: {error}"));
    }
    let source = unsafe { File::from_raw_fd(fd) };
    let metadata = source
        .metadata()
        .map_err(|error| format!("statting opened candidate: {error}"))?;
    if !metadata.is_file() {
        return Err("opened candidate is not a regular file".to_string());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err("opened candidate is not executable".to_string());
    }
    // Both the dedicated helper and a trusted system candidate must be unwritable by
    // group/other; a system candidate must additionally be exactly root-owned, while
    // the digest-pinned dedicated helper's owner gate tolerates a nested user
    // namespace (see `trusted_helper_metadata`). The helper's real integrity guarantee
    // is the packaged-build digest verified below.
    let owner_protected = match origin {
        BubblewrapOrigin::System => {
            trusted_system_candidate_metadata(metadata.uid(), metadata.permissions().mode())
        }
        BubblewrapOrigin::DedicatedHelper => {
            trusted_helper_metadata(metadata.uid(), metadata.permissions().mode())
        }
        // The bundled candidate is a sealed memfd snapshot, digest-verified below; it
        // carries no on-disk owner to gate here.
        BubblewrapOrigin::Bundled => true,
    };
    if !owner_protected {
        let label = match origin {
            BubblewrapOrigin::DedicatedHelper => "dedicated nesting helper",
            BubblewrapOrigin::System => "system candidate",
            BubblewrapOrigin::Bundled => "bundled candidate",
        };
        return Err(format!(
            "{label} is not root-owned and protected from group/other writes (uid={}, mode={:o})",
            metadata.uid(),
            metadata.permissions().mode() & 0o7777
        ));
    }
    let source_identity = (metadata.dev(), metadata.ino());
    // This path is used only to preserve bundled-helper visibility for nested
    // sandboxes under a minimal root. Execution remains pinned to `executable`.
    let source_path = fs::canonicalize(path)
        .map_err(|error| format!("resolving opened candidate path: {error}"))?;
    let executable = match origin {
        BubblewrapOrigin::System => relocate_file_at_least(source, FIRST_LAUNCH_DATA_FD)
            .map_err(|error| format!("pinning system candidate descriptor: {error}"))?,
        BubblewrapOrigin::Bundled => executable_snapshot(&source, path)?,
        // The helper is exec'd through the pinned descriptor (never re-resolved by
        // path) so a post-verification path swap cannot change what runs; but it is
        // exec'd from its REAL inode, not a memfd copy, because the path-bound
        // AppArmor `userns` profile only attaches to the real `/usr/libexec/nub`
        // path. Verify the opened fd's bytes against the packaged digest, then pin
        // that same fd for execution.
        BubblewrapOrigin::DedicatedHelper => {
            verify_pinned_helper_digest(&source, path)?;
            relocate_file_at_least(source, FIRST_LAUNCH_DATA_FD)
                .map_err(|error| format!("pinning dedicated helper descriptor: {error}"))?
        }
    };
    Ok(PinnedBubblewrapCandidate {
        executable,
        source_path,
        source_identity,
    })
}

/// Marker prefixed onto a dedicated-helper open failure caused by a group-access
/// denial (EACCES on the 0750 root:nub-sandbox helper), so the nesting classifier
/// can attribute it to missing `nub-sandbox` group membership.
const DEDICATED_HELPER_ACCESS_TAG: &str = "dedicated helper group access denied";

/// Verify the OPENED helper inode byte-for-byte against the packaged digest. Reads
/// through the already-open descriptor (no path re-resolution), so the bytes hashed
/// are the exact bytes the pinned descriptor will execute.
fn verify_pinned_helper_digest(source: &File, path: &Path) -> Result<(), String> {
    let Some(expected) = option_env!("NUB_BWRAP_SHA256") else {
        return Err(format!(
            "the running Nub was built without a packaged Bubblewrap digest, so the dedicated helper cannot be verified: {}",
            path.display()
        ));
    };
    verify_pinned_helper_digest_against(source, path, expected)
}

fn verify_pinned_helper_digest_against(
    source: &File,
    path: &Path,
    expected: &str,
) -> Result<(), String> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("packaged Bubblewrap digest is malformed".to_string());
    }
    let size = source
        .metadata()
        .map_err(|error| format!("statting the dedicated helper: {error}"))?
        .len();
    if size > MAX_BUNDLED_BWRAP_BYTES {
        return Err(format!(
            "the dedicated helper is unexpectedly large: {}",
            path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    while offset < size {
        let wanted = usize::try_from((size - offset).min(buffer.len() as u64))
            .expect("bounded read chunk fits usize");
        let read = source
            .read_at(&mut buffer[..wanted], offset)
            .map_err(|error| format!("reading the dedicated helper: {error}"))?;
        if read == 0 {
            return Err("the dedicated helper changed while it was being verified".to_string());
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "the dedicated helper does not match the packaged Bubblewrap build (digest mismatch): {}",
            path.display()
        ));
    }
    Ok(())
}

fn trusted_system_candidate_metadata(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

/// The dedicated helper's owner gate, tolerant of a nested user namespace. It admits
/// the host view (root-owned) OR a view where the file's owner is UNMAPPED and `stat`
/// therefore reports the kernel overflow uid — the exact case a level-2+ composition
/// hits when it opens the same read-only host helper from inside its own user namespace
/// (host root maps to nobody). Group/other write is forbidden in both cases (mode is
/// not uid-mapped). This owner check is defense-in-depth ONLY and is not, on its own,
/// meant to be tamper-evidence: a genuinely nobody-owned (overflow) file would also
/// pass it. The caller hard-verifies the pinned helper fd against the packaged-build
/// digest, which IS the tamper-evidence and is unaffected by the userns owner mapping —
/// so a substituted helper is caught by the digest regardless of the reported owner.
/// In the initial (host) userns a root-owned file reports uid 0, so accepting the
/// overflow uid never widens the level-1 admission (there it is genuinely uid 0).
fn trusted_helper_metadata(uid: u32, mode: u32) -> bool {
    (uid == 0 || uid == overflow_uid()) && mode & 0o022 == 0
}

/// The kernel overflow uid (`/proc/sys/kernel/overflowuid`, default 65534) — the owner
/// `stat` reports for a file whose real owner is not mapped into the current user
/// namespace.
fn overflow_uid() -> u32 {
    fs::read_to_string("/proc/sys/kernel/overflowuid")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(65534)
}

fn executable_snapshot(source: &File, path: &Path) -> Result<File, String> {
    let Some(expected) = option_env!("NUB_BWRAP_SHA256") else {
        return Err(format!(
            "bundled Bubblewrap has no build-pinned digest: {}",
            path.display()
        ));
    };
    executable_snapshot_with_digest(source, path, expected)
}

fn executable_snapshot_with_digest(
    source: &File,
    path: &Path,
    expected: &str,
) -> Result<File, String> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("build-pinned Bubblewrap digest is malformed".to_string());
    }
    let source_size = source
        .metadata()
        .map_err(|error| format!("statting bundled Bubblewrap: {error}"))?
        .len();
    if source_size > MAX_BUNDLED_BWRAP_BYTES {
        return Err(format!(
            "bundled Bubblewrap is unexpectedly large: {}",
            path.display()
        ));
    }
    let snapshot_fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"nub-bwrap-snapshot".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as RawFd
    };
    if snapshot_fd < 0 {
        return Err(format!(
            "creating immutable bundled Bubblewrap snapshot: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut snapshot = unsafe { File::from_raw_fd(snapshot_fd) };
    let mut offset = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    while offset < source_size {
        let wanted = usize::try_from((source_size - offset).min(buffer.len() as u64))
            .expect("bounded copy chunk fits usize");
        let read = source
            .read_at(&mut buffer[..wanted], offset)
            .map_err(|error| format!("reading bundled Bubblewrap snapshot source: {error}"))?;
        if read == 0 {
            return Err("bundled Bubblewrap changed while it was being snapshotted".to_string());
        }
        snapshot
            .write_all(&buffer[..read])
            .map_err(|error| format!("writing bundled Bubblewrap snapshot: {error}"))?;
        offset += read as u64;
    }
    if unsafe { libc::fchmod(snapshot.as_raw_fd(), 0o500) } != 0 {
        return Err(format!(
            "marking bundled Bubblewrap snapshot executable: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe {
        libc::fcntl(
            snapshot.as_raw_fd(),
            libc::F_ADD_SEALS,
            REQUIRED_EXECUTABLE_SNAPSHOT_SEALS,
        )
    } != 0
    {
        return Err(format!(
            "sealing bundled Bubblewrap snapshot: {}",
            std::io::Error::last_os_error()
        ));
    }
    let seals = unsafe { libc::fcntl(snapshot.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & REQUIRED_EXECUTABLE_SNAPSHOT_SEALS != REQUIRED_EXECUTABLE_SNAPSHOT_SEALS
    {
        return Err("bundled Bubblewrap snapshot did not retain every required seal".to_string());
    }
    let snapshot_size = snapshot
        .metadata()
        .map_err(|error| format!("statting bundled Bubblewrap snapshot: {error}"))?
        .len();
    if snapshot_size != source_size {
        return Err("bundled Bubblewrap snapshot has the wrong size".to_string());
    }
    let mut bytes = vec![0u8; snapshot_size as usize];
    snapshot
        .read_exact_at(&mut bytes, 0)
        .map_err(|error| format!("hashing bundled Bubblewrap snapshot: {error}"))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "bundled Bubblewrap digest mismatch for {}",
            path.display()
        ));
    }
    relocate_file_at_least(snapshot, FIRST_LAUNCH_DATA_FD)
        .map_err(|error| format!("pinning bundled Bubblewrap snapshot descriptor: {error}"))
}

fn relocate_file_at_least(file: File, minimum: RawFd) -> std::io::Result<File> {
    if file.as_raw_fd() >= minimum {
        return Ok(file);
    }
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, minimum) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Turn an empty/rejected candidate set into ONE precise diagnostic. A `nesting`
/// launch differs only where the dedicated helper is involved: its admission
/// failures — (1) missing `nub-sandbox` group access, (2) an absent helper,
/// (3) helper integrity — have no single-level counterpart, and the AppArmor
/// signal is read differently. A single-level launch blames the host restriction
/// outright; a nesting launch instead INFERS an unloaded helper profile from a
/// namespace denial, so it must first rule out the host/container causes that
/// disable unprivileged nesting regardless of any profile. Carries no privileged
/// remediation commands.
fn classify_bwrap_failures(failures: &[String], nesting: bool) -> String {
    classify_bwrap_failures_under(
        failures,
        nesting,
        apparmor_restricts_unprivileged_userns(),
        in_container(),
    )
}

/// The AppArmor restriction and the container verdict are parameters so the
/// classification is testable without the host's real sysctl value — and, since
/// part of the test suite itself runs inside containers, without the answer
/// flipping under the runner.
fn classify_bwrap_failures_under(
    failures: &[String],
    nesting: bool,
    apparmor_restricted: bool,
    containerized: bool,
) -> String {
    let detail = failures.join("; ");
    let lower = detail.to_ascii_lowercase();

    if nesting {
        if failures
            .iter()
            .any(|failure| failure.contains(DEDICATED_HELPER_ACCESS_TAG))
        {
            return format!(
                "the dedicated Bubblewrap nesting helper at {DEDICATED_HELPER_PATH} is not accessible to this user; add the user to the nub-sandbox group and start a fresh login ({detail})"
            );
        }
        // An ENOENT on the CANDIDATE OPEN specifically, not any ENOENT that a
        // later probe stage might surface.
        if lower.contains("opening candidate: no such file or directory") {
            return format!(
                "the sandbox helper is not installed at {DEDICATED_HELPER_PATH}.\n\n{}\n\n(underlying: {detail})",
                apparmor_setup_hint()
            );
        }
        if lower.contains("digest")
            || lower.contains("root-owned")
            || lower.contains("not a regular file")
            || lower.contains("not executable")
            || lower.contains("unexpectedly large")
            || lower.contains("cannot be verified")
        {
            return format!(
                "the dedicated Bubblewrap nesting helper at {DEDICATED_HELPER_PATH} failed its integrity check; reinstall the packaged helper from the documented host setup ({detail})"
            );
        }
    }

    if lower.contains("unknown option") || lower.contains("invalid option") {
        return format!("installed Bubblewrap lacks required stock options ({detail})");
    }
    // `--ro-bind-fd FD DEST` is not a descriptor bind: Bubblewrap rewrites it to a bind on
    // the literal string `/proc/self/fd/FD` and `realpath()`s that from inside the new user
    // namespace, so the source is resolved by PATH under a single-uid map. Nub re-anchors an
    // object whose path will not resolve there before it launches, which means reaching here
    // is the residual case where that repair was itself unavailable.
    //
    // Deliberately ahead of the container, sysctl, WSL and AppArmor arms, all of which would
    // otherwise claim it: this is not a namespace denial at all, so neither host setup nor a
    // container flag can fix it and naming either sends the reader somewhere useless. The
    // `lower` it matches is every candidate's failure JOINED, though, so a run that also
    // produced a real namespace denial must still report that one — hence the guard.
    if lower.contains("can't find source path /proc/self/fd/") && !is_namespace_denial(&lower) {
        return format!(
            "nub could not mount its own runtime into the sandbox: Bubblewrap resolves the \
             mount by path from inside the user namespace, and that path is unreachable \
             there. Install nub under a directory every user can search, such as \
             /usr/local/bin, or make /tmp writable and executable ({detail})"
        );
    }
    if fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        .is_ok_and(|value| value.trim() == "0")
    {
        return format!("unprivileged user namespaces are disabled ({detail})");
    }
    if fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|value| value.to_ascii_lowercase().contains("microsoft"))
    {
        return format!("Bubblewrap cannot create the required views under WSL ({detail})");
    }
    if containerized {
        return format!("the container policy blocks required Bubblewrap behavior ({detail})");
    }
    // BOTH launch modes land here, and the container/sysctl/WSL arms above must
    // come first: a namespace denial inside a container on an AppArmor-restricted
    // host is the container's doing, and host setup run in there fixes nothing —
    // pointing at it names the wrong cause. Past that, a loaded helper profile
    // would have permitted the namespace, so a surviving denial means the grant is
    // not in effect. (Enumerating loaded profiles via securityfs needs privilege,
    // so it is inferred from the denial, not read.)
    if apparmor_restricted && is_namespace_denial(&lower) {
        if nesting {
            return format!(
                "the sandbox helper's AppArmor profile is not loaded, so the host blocks the user namespaces the sandbox needs.\n\n{}\n\n(underlying: {detail})",
                apparmor_setup_hint()
            );
        }
        return format!("{}\n\n(underlying: {detail})", apparmor_setup_hint());
    }
    format!("Bubblewrap cannot enforce the required process view ({detail})")
}

fn in_container() -> bool {
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || fs::read_to_string("/proc/1/cgroup").is_ok_and(|value| {
            ["docker", "containerd", "kubepods", "libpod"]
                .iter()
                .any(|marker| value.contains(marker))
        })
}

/// The remedy line for the AppArmor userns restriction, naming the exact copy-pasteable setup
/// command and the CI one-liner. Kept in one place so every fail-closed path that the setup
/// fixes points the user at the same command (epic C1).
///
/// The `sudo -n` probe only REPORTS that the machine could repair itself without a prompt; nub
/// never elevates on its own initiative.
fn apparmor_setup_hint() -> String {
    apparmor_setup_hint_with(crate::backend::linux_setup::passwordless_sudo_available())
}

fn apparmor_setup_hint_with(passwordless_sudo: bool) -> String {
    use crate::backend::linux_setup::{SETUP_COMMAND, SETUP_COMMAND_ALL_USERS};
    let base = format!(
        "the sandbox needs a one-time setup on this system. Ubuntu's kernel restricts the user \
         namespaces the sandbox relies on; grant nub's bundled bubblewrap the one capability it \
         needs (the only step that needs root, paid once per machine):\n\n    {SETUP_COMMAND}\n\n\
         This installs a digest-pinned copy of bubblewrap at {DEDICATED_HELPER_PATH} and an \
         AppArmor profile keyed to it; the global restriction stays on for everything else. That \
         setup gates the helper behind the nub-sandbox group, which an already-running shell or \
         CI job cannot pick up — on an ephemeral or single-user machine use \
         `{SETUP_COMMAND_ALL_USERS}` instead and the very next command works."
    );
    if passwordless_sudo {
        return format!(
            "{base}\n\nPasswordless sudo works here, so this needs no prompt:\n\n    \
             {SETUP_COMMAND_ALL_USERS}"
        );
    }
    base
}

fn apparmor_restricts_unprivileged_userns() -> bool {
    fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|value| value.trim() == "1")
}

/// A namespace / credential-setup denial in a Bubblewrap failure string. The exact
/// wording varies by Bubblewrap version ("setting up uid map: Permission denied",
/// "No permissions to create a new namespace"), so this matches on the substantive
/// tokens rather than whole sentences. `RTM_NEWADDR` is the netns-loopback
/// bring-up: on a restricted Ubuntu host `--unshare-all` dies THERE first, before
/// reaching any uid-map step, which is why the errno alone is not enough to key on.
///
/// A BARE errno is deliberately not a match. Matching "permission denied" on its own also
/// caught an ordinary bind-mount EACCES — an unreadable path, a mode the caller does not hold
/// — and sent that caller to `sudo nub setup-sandbox`, which cannot help: no AppArmor grant
/// makes an unreadable path readable. The errno has to sit alongside a named namespace or
/// credential operation to mean what the setup hint claims it means.
///
/// The list spans both places a launch first needs the namespace:
///
/// - the namespace/credential operations proper. `unshare` covers the SECOND-level
///   namespace, which is not an exotic path: `--dev` sets Bubblewrap's `opt_needs_devpts`,
///   which makes the first level map the caller to uid 0 so devpts can mount, so every
///   launch by a NON-root user creates a second one to map back. Nub passes `--dev` always.
/// - the mount steps (`make / slave`, `mount tmpfs`, `newroot bind`, `pivot_root`), where an
///   fs-only policy dies instead, because `--unshare-net` is conditional on
///   `policy.net.enforce` and the loopback bring-up that yields `RTM_NEWADDR` never runs.
fn is_namespace_denial(lower: &str) -> bool {
    // `rtm_new` rather than `rtm_newaddr`: the netns loopback bring-up emits RTM_NEWADDR first
    // and RTM_NEWLINK right behind it, and both are the same denial with the same remedy.
    // `setgroups` is the credential half — bubblewrap's "error writing to setgroups" names no
    // namespace and would otherwise miss. The substring forms (`namespace`, `unshare`) also
    // subsume the narrower spellings the fs-only mount steps below do not cover.
    const NAMESPACE_OPERATIONS: [&str; 11] = [
        "uid map",
        "gid map",
        "namespace",
        "userns",
        "unshare",
        "rtm_new",
        "setgroups",
        "make / slave",
        "mount tmpfs",
        "newroot bind",
        "pivot_root",
    ];
    NAMESPACE_OPERATIONS
        .iter()
        .any(|operation| lower.contains(operation))
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn open_inheritable_dev_null() -> std::io::Result<File> {
    file_above_stdio(File::open("/dev/null")?)
}

fn file_above_stdio(file: File) -> std::io::Result<File> {
    if file.as_raw_fd() >= 3 {
        return Ok(file);
    }
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn write_bwrap_arguments<'a>(args: impl Iterator<Item = &'a OsStr>) -> std::io::Result<File> {
    let mut bytes = Vec::new();
    for arg in args {
        if arg.as_bytes().contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a Bubblewrap argument contains a NUL byte",
            ));
        }
        bytes.extend_from_slice(arg.as_bytes());
        bytes.push(0);
    }
    sealed_support_file("nub-bwrap-arguments", &bytes)
}

fn sealed_support_file(name: &str, bytes: &[u8]) -> std::io::Result<File> {
    let name = std::ffi::CString::new(name).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "memfd name contains NUL")
    })?;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as RawFd
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = file_above_stdio(unsafe { File::from_raw_fd(fd) })?;
    file.write_all(bytes)?;
    file.rewind()?;
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            REQUIRED_EXECUTABLE_SNAPSHOT_SEALS,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

struct SandboxSyscalls {
    socket: i64,
    io_uring_setup: i64,
    io_uring_enter: i64,
    io_uring_register: i64,
    keyctl: i64,
    add_key: i64,
    request_key: i64,
    setxattr: i64,
    lsetxattr: i64,
    removexattr: i64,
    lremovexattr: i64,
    fchownat: i64,
    /// x86_64 keeps the legacy path-based `chown`/`lchown`; arm64's generic syscall ABI
    /// dropped them, leaving `fchownat` as glibc's only path-form entry point — hence
    /// `None` there rather than a number that does not exist.
    chown: Option<i64>,
    lchown: Option<i64>,
}

#[cfg(test)]
impl Default for SandboxSyscalls {
    /// x86_64's real numbers, so a test literal can spread in the fields it does not
    /// exercise. Real numbers rather than zeros, because a stray `0` would mean `read`.
    fn default() -> Self {
        Self {
            socket: 41,
            io_uring_setup: 425,
            io_uring_enter: 426,
            io_uring_register: 427,
            keyctl: 250,
            add_key: 248,
            request_key: 249,
            setxattr: 188,
            lsetxattr: 189,
            removexattr: 197,
            lremovexattr: 198,
            fchownat: 260,
            chown: Some(92),
            lchown: Some(94),
        }
    }
}

/// Whether the socket ceiling admits the IP families (`AF_INET`/`AF_INET6`). Deliberately NOT
/// spelled `per_host`, which is what this parameter used to be called: the two callers that ask
/// for `Permitted` want the same two families for UNRELATED reasons, and reading the flag as
/// "the proxy/netns tier is active" is now wrong.
///
/// - the retained-monitor path asks because the child sits in an EMPTY netns whose only route
///   out is a bridge to nub's proxy, so an IP socket reaches the proxy and nothing else;
/// - the Landlock build jail asks because the catalog GRANTED this package egress and there is
///   no netns to route through — the grant is coarse (see [`apply_landlock`]).
///
/// It never widens past those two families, so neither caller can turn it into a general
/// socket escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IpEgress {
    Denied,
    Permitted,
}

pub(super) fn build_seccomp(
    restrict_network: bool,
    ip_egress: IpEgress,
    deny_keyring: bool,
    permit_keyring_join: bool,
    deny_metadata: bool,
) -> Result<Option<BpfProgram>, String> {
    if !restrict_network && !deny_keyring && !deny_metadata {
        return Ok(None);
    }
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for sandbox filter: {e}"))?;
    build_seccomp_for(
        arch,
        restrict_network,
        ip_egress,
        deny_keyring,
        permit_keyring_join,
        deny_metadata,
        SandboxSyscalls {
            socket: libc::SYS_socket,
            io_uring_setup: libc::SYS_io_uring_setup,
            io_uring_enter: libc::SYS_io_uring_enter,
            io_uring_register: libc::SYS_io_uring_register,
            keyctl: libc::SYS_keyctl,
            add_key: libc::SYS_add_key,
            request_key: libc::SYS_request_key,
            setxattr: libc::SYS_setxattr,
            lsetxattr: libc::SYS_lsetxattr,
            removexattr: libc::SYS_removexattr,
            lremovexattr: libc::SYS_lremovexattr,
            fchownat: libc::SYS_fchownat,
            #[cfg(target_arch = "x86_64")]
            chown: Some(libc::SYS_chown),
            #[cfg(not(target_arch = "x86_64"))]
            chown: None,
            #[cfg(target_arch = "x86_64")]
            lchown: Some(libc::SYS_lchown),
            #[cfg(not(target_arch = "x86_64"))]
            lchown: None,
        },
    )
    .map(Some)
}

fn build_seccomp_for(
    arch: TargetArch,
    restrict_network: bool,
    ip_egress: IpEgress,
    deny_keyring: bool,
    permit_keyring_join: bool,
    deny_metadata: bool,
    syscalls: SandboxSyscalls,
) -> Result<BpfProgram, String> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    if restrict_network {
        // THE SOCKET-FAMILY CEILING, and [`IpEgress::Permitted`] lifts EXACTLY the two IP
        // families out of it — never any of the rest. That asymmetry is the invariant: the
        // non-IP families are not an egress question, so no caller has grounds to relax them
        // and none may. AF_UNIX reaches host daemons through a filesystem path that neither a
        // netns nor Landlock scopes (`connect` on a socket file is not an `open`, so no fs rule
        // mediates it); AF_VSOCK reaches the hypervisor by CID; AF_BLUETOOTH/AF_RDS/AF_CAN/
        // AF_TIPC/AF_IB/AF_NFC are global buses. AF_PACKET/AF_XDP are refused because nothing a
        // dependency build legitimately does needs a raw socket. AF_NETLINK is deliberately
        // ABSENT from the list so a nested Bubblewrap can configure its own private netns
        // without reaching the host network, and `socketpair(2)` is unfiltered so in-process
        // local IPC still works. io_uring is blocked below at all three entry points, which is
        // what keeps a socket from being created off this filter entirely.
        const IP_FAMILIES: [libc::c_int; 2] = [libc::AF_INET, libc::AF_INET6];
        let all_denied = [
            libc::AF_UNIX,
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_PACKET,
            libc::AF_VSOCK,
            libc::AF_XDP,
            libc::AF_BLUETOOTH,
            libc::AF_RDS,
            libc::AF_CAN,
            libc::AF_TIPC,
            libc::AF_IB,
            libc::AF_NFC,
        ];
        let denied_families: Vec<libc::c_int> = all_denied
            .into_iter()
            .filter(|family| !(ip_egress == IpEgress::Permitted && IP_FAMILIES.contains(family)))
            .collect();
        let mut socket_rules = Vec::with_capacity(denied_families.len());
        for family in denied_families {
            socket_rules.push(
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        0,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::Eq,
                        family as u64,
                    )
                    .map_err(|e| format!("network-family condition: {e}"))?,
                ])
                .map_err(|e| format!("network-family rule: {e}"))?,
            );
        }
        rules.insert(syscalls.socket, socket_rules);

        // io_uring can create sockets without issuing socket(2). Blocking only
        // io_uring_setup would leave the ring's SECOND entry point open: a process
        // holding an already-created ring fd submits through io_uring_enter, and
        // registers buffers/files through io_uring_register, without ever calling
        // setup itself. That matters most for AF_VSOCK, which the network namespace
        // does not confine (see the family carve-out above) — there this filter is
        // the only boundary, so all three entry points are denied as one set.
        for syscall in [
            syscalls.io_uring_setup,
            syscalls.io_uring_enter,
            syscalls.io_uring_register,
        ] {
            rules.insert(syscall, Vec::new());
        }
    }

    if deny_keyring {
        // add_key/request_key have no legitimate in-sandbox use — deny wholesale.
        for syscall in [syscalls.add_key, syscalls.request_key] {
            rules.insert(syscall, Vec::new());
        }
        if permit_keyring_join {
            // A NESTING launch's keyctl is denied EXCEPT the anonymous session-keyring
            // join `keyctl(KEYCTL_JOIN_SESSION_KEYRING, NULL)` — the exact isolation
            // primitive the nested monitor uses (under THIS inherited filter) to hand
            // ITS child a fresh EMPTY session keyring. Without the carve-out a nested
            // monitor cannot establish its own isolation, so composition fails closed
            // (the "inherited keyring seccomp must still permit creating the next
            // monitor" invariant). It leaks nothing: a NULL-name join yields an empty
            // keyring, while every credential read/search/update and any NAMED join
            // (which could re-attach an ancestor keyring holding credentials) still
            // EPERMs. Rules are OR'd, so keyctl matches -> EPERM whenever the option is
            // not the join OR the name pointer is non-NULL; only `(join, NULL)` Allows.
            // A single-level launch (permit_keyring_join=false) keeps the strict
            // deny-all below, so its filter is byte-identical to the pre-nesting one.
            rules.insert(
                syscalls.keyctl,
                vec![
                    SeccompRule::new(vec![
                        SeccompCondition::new(
                            0,
                            SeccompCmpArgLen::Dword,
                            SeccompCmpOp::Ne,
                            libc::KEYCTL_JOIN_SESSION_KEYRING as u64,
                        )
                        .map_err(|e| format!("keyring option condition: {e}"))?,
                    ])
                    .map_err(|e| format!("keyring option rule: {e}"))?,
                    SeccompRule::new(vec![
                        SeccompCondition::new(1, SeccompCmpArgLen::Qword, SeccompCmpOp::Ne, 0)
                            .map_err(|e| format!("keyring name condition: {e}"))?,
                    ])
                    .map_err(|e| format!("keyring name rule: {e}"))?,
                ],
            );
        } else {
            rules.insert(syscalls.keyctl, Vec::new());
        }
    }

    if deny_metadata {
        // Landlock has no metadata hook at ANY ABI, so ownership and xattr rewriting is
        // otherwise unmediated here — see `drop_all_capabilities`, which handles the
        // capability half of the same problem. seccomp is the only other lever without a
        // mount namespace, and this is the subset of it that costs nothing.
        //
        // WHAT IS ABSENT MATTERS MORE THAN WHAT IS PRESENT. A denial matrix over five real
        // native installs (better-sqlite3, sqlite3, esbuild, simple-git-hooks, bufferutil)
        // on kernel 6.8 found these two families free at BOTH uid 1000 and root, while:
        //   - `chmod`/`fchmodat` breaks 4 of 5 with EPERM — node-gyp chmods the built addon
        //     from inside a make recipe, so denying it kills every from-source build.
        //   - `utimensat` breaks sqlite3 and bufferutil, in either the path or the fd form.
        // Both were proposed off an strace showing zero calls and falsified by actually
        // denying them. Anything added here needs that matrix re-run, not a trace.
        //
        // The fd forms (`fchown`, `fsetxattr`) are deliberately absent. As root, node-tar's
        // `preserveOwner` flips on and the extractors chown heavily through them; the path
        // forms below are attempted too (6 `fchownat` calls in a cold-cache root install of
        // sqlite3) but every one is best-effort and swallowed, so EPERM there costs nothing
        // while EPERM on the fd form is untested and needlessly risks a root regression.
        //
        // Honest value: these two families are safe to deny because nothing uses them, and
        // nothing uses them because they achieve little — chown to another uid already fails
        // under DAC, and `user.*` xattrs are inert. This narrows the metadata surface; the
        // residual it leaves (host-wide `chmod` on anything the jailed uid owns, plus
        // arbitrary mtime rewriting) is the part with teeth, and it survives intact.
        // See wiki/design/build-jail-linux.md.
        for syscall in [
            syscalls.setxattr,
            syscalls.lsetxattr,
            syscalls.removexattr,
            syscalls.lremovexattr,
            syscalls.fchownat,
        ]
        .into_iter()
        .chain(syscalls.chown)
        .chain(syscalls.lchown)
        {
            rules.insert(syscall, Vec::new());
        }
    }

    let program = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("building sandbox filter: {e}"))?
    .try_into()
    .map_err(|e| format!("compiling sandbox filter: {e}"))?;
    if arch == TargetArch::x86_64 {
        prepend_x86_64_unsupported_abi_guard(program)
    } else {
        Ok(program)
    }
}

fn prepend_x86_64_unsupported_abi_guard(mut program: BpfProgram) -> Result<BpfProgram, String> {
    const MAX_BPF_INSTRUCTIONS: usize = 4096;
    const LEGACY_CONFUSED_ABI_FIRST: u32 = 512;
    const LEGACY_CONFUSED_ABI_LAST: u32 = 547;

    let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
    let guard = [
        // Load seccomp_data.arch.
        sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 4,
        },
        // A foreign architecture skips to seccompiler's own arch check and KILL action.
        sock_filter {
            code: 0x15,
            jt: 0,
            jf: 6,
            k: AUDIT_ARCH_X86_64,
        },
        // Load seccomp_data.nr.
        sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 0,
        },
        // Reject every syscall carrying the unsupported x32 ABI bit.
        sock_filter {
            code: 0x35,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: denied,
        },
        // Linux before 5.4 also accepted confused x32 encodings 512..=547.
        sock_filter {
            code: 0x35,
            jt: 0,
            jf: 2,
            k: LEGACY_CONFUSED_ABI_FIRST,
        },
        sock_filter {
            code: 0x25,
            jt: 1,
            jf: 0,
            k: LEGACY_CONFUSED_ABI_LAST,
        },
        sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: denied,
        },
    ];
    let guarded_len = guard.len() + program.len();
    if guarded_len > MAX_BPF_INSTRUCTIONS {
        return Err(format!(
            "sandbox filter has {guarded_len} instructions, above the kernel limit of {MAX_BPF_INSTRUCTIONS}"
        ));
    }
    let mut guarded = Vec::with_capacity(guarded_len);
    guarded.extend(guard);
    guarded.append(&mut program);
    Ok(guarded)
}

#[cfg(test)]
unsafe fn cloexec_open_fds_from_proc() -> std::io::Result<()> {
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
        return Err(std::io::Error::last_os_error());
    }

    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory, stat.as_mut_ptr()) } != 0
        || unsafe { stat.assume_init() }.f_type != PROC_SUPER_MAGIC
    {
        unsafe { libc::close(directory) };
        return Err(std::io::Error::other(
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
            let error = std::io::Error::last_os_error();
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
                return Err(std::io::Error::other(
                    "procfs descriptor enumeration returned a truncated record",
                ));
            }
            let record = &buffer[offset..count];
            let reclen = u16::from_ne_bytes([record[16], record[17]]) as usize;
            if reclen < DIRENT_HEADER || offset + reclen > count {
                unsafe { libc::close(directory) };
                return Err(std::io::Error::other(
                    "procfs descriptor enumeration returned a malformed record",
                ));
            }
            let name = &record[DIRENT_HEADER..reclen];
            let Some(end) = name.iter().position(|byte| *byte == 0) else {
                unsafe { libc::close(directory) };
                return Err(std::io::Error::other(
                    "procfs descriptor enumeration returned an unterminated name",
                ));
            };
            let name = &name[..end];
            if name != b"." && name != b".." {
                let mut fd: RawFd = 0;
                if name.is_empty() {
                    unsafe { libc::close(directory) };
                    return Err(std::io::Error::other(
                        "procfs descriptor enumeration returned an empty name",
                    ));
                }
                for byte in name {
                    if !byte.is_ascii_digit() {
                        unsafe { libc::close(directory) };
                        return Err(std::io::Error::other(
                            "procfs descriptor enumeration returned a nonnumeric name",
                        ));
                    }
                    fd = fd
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(i32::from(*byte - b'0')))
                        .ok_or_else(|| {
                            std::io::Error::other(
                                "procfs descriptor enumeration overflowed a descriptor number",
                            )
                        })?;
                }
                if fd == directory {
                    saw_directory = true;
                }
                if fd >= 3 && fd != directory {
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    if flags < 0
                        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
                    {
                        let error = std::io::Error::last_os_error();
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
        return Err(std::io::Error::other(
            "procfs descriptor enumeration omitted its own descriptor",
        ));
    }
    Ok(())
}

/// Launch under Landlock + seccomp — no namespace, no helper process, no bubblewrap.
///
/// NETWORK IS A PER-PACKAGE BOOLEAN HERE, and that boolean is the whole contract rather than a
/// shortfall against a richer tier. A package the catalog names has `AF_INET`/`AF_INET6` lifted
/// out of the socket ceiling; a package it does not name keeps the full ceiling and reaches
/// nothing. Both directions are simply what `compiler::preset::build_jail_net` compiled, read
/// back off the IR by [`ip_egress_for`].
///
/// WHY A COARSE FAMILY PERMIT IS SOUND WITH NO NETNS — and why the host list is not a gate.
/// Per-host egress requires the child's ONLY route out to be nub's proxy, which requires an
/// empty network namespace, which requires an unprivileged user namespace: the one thing this
/// product cannot demand (Ubuntu 24.04 denies it by default, and refusing to install there is
/// not an option). Absent a netns, a permitted `AF_INET` dials any host directly, so no host list
/// could be enforced here — which is why `build_jail_net` emits none, on any platform. The
/// defense that survives is the one aimed at the actual attack: an unvetted package — the
/// Shai-Hulud shape, a `postinstall` published into something nobody reviewed — has no catalog
/// entry and therefore gets zero egress. A coarse permit is only ever handed to a package a
/// pull request already admitted.
///
/// The residual is named rather than hidden: a granted package can reach loopback and any host at
/// all. That is accepted — the jail is defense in depth, and the exposure is bounded to the
/// reviewed set. What is NOT accepted is what this comment used to assert,
/// that prefetch serves every package needing a remote artifact. It does not: prefetch covers
/// `prebuild-install`/`node-pre-gyp` artifacts, while 181 of the 344-package corpus fetch
/// beyond that, so a blanket deny broke them instead of confining them.
fn apply_landlock(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    plan: LandlockPreflight,
    tmp_dir: Option<&Path>,
) -> Result<Prepared, Degradation> {
    let seccomp = build_seccomp(
        policy.net.enforce,
        ip_egress_for(&policy.net),
        protects_ambient_credentials(policy),
        false,
        // Metadata denial is scoped to THIS backend, which is the build jail's only
        // mechanism. `nub sandbox` runs commands the user chose, where a refused chown
        // would be a surprise; a dependency's install script has no comparable claim.
        true,
    )
    .map_err(|reason| Degradation {
        lost: vec!["net".to_string()],
        reason: Some(reason),
    })?;

    let mut command = base_command(&spec, policy);
    // No mount namespace means no `/tmp` rebind, so the child is pointed at the per-run
    // scratch dir by its real host path instead.
    if let Some(tmp) = tmp_dir {
        command.env("TMPDIR", tmp);
    }
    // Resolve the entry program the same way the bubblewrap path does, so it can be granted
    // in its own right even when it lives outside the system read floor.
    let child_cwd = spec
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let entry_program = resolve_program(&spec.program, &child_cwd, target_path(policy).as_deref());

    let (command, ruleset) = super::linux_landlock::prepare_launch(
        policy,
        command,
        seccomp,
        tmp_dir,
        entry_program.as_deref(),
    )
    .map_err(|reason| Degradation {
        lost: vec!["fs".to_string()],
        reason: Some(reason),
    })?;

    tracing::debug!(
        abi = plan.abi,
        rules = ruleset.rules_added,
        "confining lifecycle spawn with landlock"
    );

    Ok(Prepared {
        command,
        // Fully enforced, and the per-package boolean is what "fully" means on this axis — see
        // the fn doc. NOT reported as a lost `net-per-host`: the catalog's documented contract
        // IS the boolean (`data/build-jail-catalog.json` `enforcementStatus`), so there is no
        // host-granularity promise to fall short of, and a per-spawn "reduced mode" warning on
        // every one of the 181 granted packages would be noise asserting something false.
        degradation: Degradation::full(),
        proxy: None,
        net_bridge: None,
        // Holds the ruleset descriptor open until the child is spawned; `pre_exec` consumes
        // it after fork, so dropping it any earlier would leave the hook restricting nothing.
        _inherited_files: vec![std::fs::File::from(ruleset.into_fd())],
        retained_monitor: None,
        // The Landlock hook makes the child a session leader, so its descendants are
        // reachable as a process group — this path's only handle on them.
        signal_process_group: true,
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

/// The socket ceiling the compiled net axis asks for, read out of the IR.
///
/// An Allow rule IS the catalog verdict: `build_jail_net` emits a catch-all `["*"]` for a package
/// the catalog names and `false` for one it does not. Deriving from the IR rather than re-consulting
/// the catalog keeps this backend a pure IR translator, so it cannot grant something the compiled
/// policy did not. A relaxed axis (`default_effect == Allow`) counts too — it admits every host,
/// which no build-jail policy emits, but reading it as a deny would UNDER-permit a policy that
/// says "everything".
fn ip_egress_for(net: &crate::policy::NetPolicy) -> IpEgress {
    let admits_anything = net.default_effect == Effect::Allow
        || net.rules.iter().any(|rule| rule.effect == Effect::Allow);
    if admits_anything {
        IpEgress::Permitted
    } else {
        IpEgress::Denied
    }
}

fn base_command(spec: &CommandSpec, policy: &SandboxPolicy) -> Command {
    let mut command = Command::new(&spec.program);
    spec.args.apply_to(&mut command);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command.env_clear();
    for (key, value) in &policy.env.constructed {
        command.env(key, value);
    }
    command
}

fn target_path(policy: &SandboxPolicy) -> Option<OsString> {
    policy.env.constructed.get("PATH").map(OsString::from)
}

fn resolve_program(program: &OsStr, child_cwd: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() || p.components().count() > 1 {
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            child_cwd.join(p)
        };
        return executable(&candidate)
            .then(|| fs::canonicalize(candidate).ok())
            .flatten();
    }
    std::env::split_paths(path?).find_map(|dir| {
        let dir = if dir.is_absolute() {
            dir
        } else {
            child_cwd.join(dir)
        };
        let candidate = dir.join(p);
        executable(&candidate)
            .then(|| fs::canonicalize(candidate).ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::{CompileCtx, ScopeCapabilities, compile};
    use crate::matcher::path::Homes;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::os::unix::process::CommandExt;
    use tempfile::tempdir;

    /// The minimal-root floor is a system-START set, not a convenience set. Both halves of
    /// that are load-bearing and each fails differently: drop `/etc/ld.so.cache` or the GCC
    /// tree and every confined native build breaks, but keep `/etc` and `/opt` wholesale and
    /// the floor re-admits the credential surface the read-set measurement found readable by
    /// any dependency's install script running as root — which is the normal case in a
    /// container and on a CI runner, where file mode blocks nothing.
    ///
    /// Asserted as coverage rather than as a literal list copy, so it pins the CONTRACT and
    /// not the spelling: reordering or adding a genuinely-essential path keeps it green.
    #[test]
    fn the_essential_read_floor_excludes_the_credential_surface_it_used_to_mount() {
        let covered = |p: &str| {
            ESSENTIAL_READ_PATHS
                .iter()
                .any(|root| p == *root || p.starts_with(&format!("{root}/")))
        };
        for needed in [
            "/usr/lib/gcc/aarch64-linux-gnu/12/cc1plus",
            "/usr/include/stdio.h",
            "/lib/aarch64-linux-gnu/libc.so.6",
            "/etc/ld.so.cache",
            "/etc/nsswitch.conf",
            "/etc/passwd",
            "/etc/resolv.conf",
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/openssl.cnf",
        ] {
            assert!(covered(needed), "the floor must still reach {needed}");
        }
        for withheld in [
            "/etc/kubernetes/admin.conf",
            "/etc/rancher/k3s/k3s.yaml",
            "/etc/ssl/private/site.key",
            "/etc/docker/config.json",
            "/etc/shadow",
            "/opt/vendorware/creds.txt",
            // An interpreter under `/opt` is NOT stranded by this: it is mounted by its own
            // policy grant or as the entry program, never by the floor.
            "/opt/hostedtoolcache/node/22.15.0/x64/bin/node",
        ] {
            assert!(!covered(withheld), "the floor must not mount {withheld}");
        }
    }

    fn policy(root: &Path, surface: serde_json::Value) -> SandboxPolicy {
        let homes = Homes {
            home: root.join("home"),
            tmp: root.join("tmp"),
            cache: root.join("cache"),
            project: root.join("project"),
        };
        compile(
            &surface,
            &CompileCtx::new(
                homes,
                root.join("project"),
                ScopeCapabilities::approved(),
                BTreeMap::new(),
            ),
        )
        .unwrap()
    }

    fn seccomp_data_arg1(
        syscall: u32,
        audit_arch: u32,
        argument_zero: u64,
        argument_one: u64,
    ) -> [u8; 64] {
        let mut data = seccomp_data(syscall, audit_arch, argument_zero);
        data[24..32].copy_from_slice(&argument_one.to_ne_bytes());
        data
    }

    fn seccomp_data(syscall: u32, audit_arch: u32, argument_zero: u64) -> [u8; 64] {
        let mut data = [0_u8; 64];
        data[0..4].copy_from_slice(&syscall.to_ne_bytes());
        data[4..8].copy_from_slice(&audit_arch.to_ne_bytes());
        data[16..24].copy_from_slice(&argument_zero.to_ne_bytes());
        data
    }

    fn evaluate_bpf(program: &BpfProgram, data: &[u8; 64]) -> u32 {
        let mut accumulator = 0_u32;
        let mut pc = 0_usize;
        loop {
            let instruction = program
                .get(pc)
                .unwrap_or_else(|| panic!("BPF program counter {pc} is out of bounds"));
            match instruction.code {
                // BPF_LD | BPF_W | BPF_ABS
                0x20 => {
                    let offset = instruction.k as usize;
                    accumulator = u32::from_ne_bytes(
                        data[offset..offset + 4]
                            .try_into()
                            .expect("BPF load must fit in seccomp_data"),
                    );
                    pc += 1;
                }
                // BPF_ALU | BPF_AND | BPF_K
                0x54 => {
                    accumulator &= instruction.k;
                    pc += 1;
                }
                // BPF_JMP | BPF_JA
                0x05 => pc += 1 + instruction.k as usize,
                // BPF_JMP | BPF_JEQ | BPF_K
                0x15 => {
                    pc += 1 + usize::from(if accumulator == instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    });
                }
                // BPF_JMP | BPF_JGT | BPF_K
                0x25 => {
                    pc += 1 + usize::from(if accumulator > instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    });
                }
                // BPF_JMP | BPF_JGE | BPF_K
                0x35 => {
                    pc += 1 + usize::from(if accumulator >= instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    });
                }
                // BPF_RET | BPF_K
                0x06 => return instruction.k,
                code => panic!("unsupported BPF instruction 0x{code:02x} at {pc}"),
            }
        }
    }

    #[test]
    fn composed_seccomp_rejects_unsupported_abis_before_native_dispatch() {
        const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
        const AUDIT_ARCH_RISCV64: u32 = 0xc000_00f3;
        const X86_64_SOCKET: u32 = 41;
        const X86_64_GETPID: u32 = 39;
        const X86_64_ADD_KEY: u32 = 248;
        const X86_64_REQUEST_KEY: u32 = 249;
        const X86_64_KEYCTL: u32 = 250;
        const GENERIC_SOCKET: u32 = 198;
        const GENERIC_GETPID: u32 = 172;
        const GENERIC_ADD_KEY: u32 = 217;
        const GENERIC_REQUEST_KEY: u32 = 218;
        const GENERIC_KEYCTL: u32 = 219;
        const IO_URING_SETUP: u32 = 425;
        const IO_URING_ENTER: u32 = 426;
        const IO_URING_REGISTER: u32 = 427;

        let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
        let allowed = u32::from(SeccompAction::Allow);
        let killed = u32::from(SeccompAction::KillProcess);
        let x86 = build_seccomp_for(
            TargetArch::x86_64,
            true,
            IpEgress::Denied,
            true,
            false,
            false,
            SandboxSyscalls {
                socket: i64::from(X86_64_SOCKET),
                io_uring_setup: i64::from(IO_URING_SETUP),
                io_uring_enter: i64::from(IO_URING_ENTER),
                io_uring_register: i64::from(IO_URING_REGISTER),
                keyctl: i64::from(X86_64_KEYCTL),
                add_key: i64::from(X86_64_ADD_KEY),
                request_key: i64::from(X86_64_REQUEST_KEY),
                ..Default::default()
            },
        )
        .unwrap();
        for (syscall, family) in [
            (X86_64_SOCKET, libc::AF_INET),
            (IO_URING_SETUP, 0),
            (IO_URING_ENTER, 0),
            (IO_URING_REGISTER, 0),
            (X86_64_KEYCTL, 0),
            (X86_64_ADD_KEY, 0),
            (X86_64_REQUEST_KEY, 0),
            (X86_64_SOCKET | X32_SYSCALL_BIT, libc::AF_INET),
            (X86_64_SOCKET | X32_SYSCALL_BIT, libc::AF_NETLINK),
            (IO_URING_SETUP | X32_SYSCALL_BIT, 0),
            (IO_URING_ENTER | X32_SYSCALL_BIT, 0),
            (IO_URING_REGISTER | X32_SYSCALL_BIT, 0),
            (X86_64_GETPID | X32_SYSCALL_BIT, 0),
            (u32::MAX, 0),
        ] {
            assert_eq!(
                evaluate_bpf(
                    &x86,
                    &seccomp_data(syscall, AUDIT_ARCH_X86_64, family as u64),
                ),
                denied,
                "x86-64 syscall {syscall:#x} escaped the sandbox filter",
            );
        }
        for syscall in 512..=547 {
            assert_eq!(
                evaluate_bpf(&x86, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                denied,
                "legacy confused-ABI syscall {syscall} escaped the network filter",
            );
        }
        for syscall in [511, 548, X86_64_GETPID] {
            assert_eq!(
                evaluate_bpf(&x86, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                allowed,
                "native syscall {syscall} did not reach seccompiler dispatch",
            );
        }
        assert_eq!(
            evaluate_bpf(
                &x86,
                &seccomp_data(X86_64_SOCKET, AUDIT_ARCH_X86_64, libc::AF_NETLINK as u64),
            ),
            allowed,
        );
        assert_eq!(
            evaluate_bpf(&x86, &seccomp_data(X86_64_GETPID, AUDIT_ARCH_AARCH64, 0)),
            killed,
        );

        for (arch, audit_arch) in [
            (TargetArch::aarch64, AUDIT_ARCH_AARCH64),
            (TargetArch::riscv64, AUDIT_ARCH_RISCV64),
        ] {
            let program = build_seccomp_for(
                arch,
                true,
                IpEgress::Denied,
                true,
                false,
                false,
                SandboxSyscalls {
                    socket: i64::from(GENERIC_SOCKET),
                    io_uring_setup: i64::from(IO_URING_SETUP),
                    io_uring_enter: i64::from(IO_URING_ENTER),
                    io_uring_register: i64::from(IO_URING_REGISTER),
                    keyctl: i64::from(GENERIC_KEYCTL),
                    add_key: i64::from(GENERIC_ADD_KEY),
                    request_key: i64::from(GENERIC_REQUEST_KEY),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                evaluate_bpf(
                    &program,
                    &seccomp_data(GENERIC_SOCKET, audit_arch, libc::AF_INET as u64),
                ),
                denied,
            );
            for syscall in [IO_URING_SETUP, IO_URING_ENTER, IO_URING_REGISTER] {
                assert_eq!(
                    evaluate_bpf(&program, &seccomp_data(syscall, audit_arch, 0)),
                    denied,
                    "io_uring entry point {syscall} escaped the filter",
                );
            }
            for syscall in [GENERIC_KEYCTL, GENERIC_ADD_KEY, GENERIC_REQUEST_KEY] {
                assert_eq!(
                    evaluate_bpf(&program, &seccomp_data(syscall, audit_arch, 0)),
                    denied,
                );
            }
            for syscall in [512, 547, GENERIC_SOCKET | X32_SYSCALL_BIT, u32::MAX] {
                assert_eq!(
                    evaluate_bpf(&program, &seccomp_data(syscall, audit_arch, 0)),
                    allowed,
                    "non-x86 filter unexpectedly guarded syscall {syscall:#x}",
                );
            }
            assert_eq!(
                evaluate_bpf(
                    &program,
                    &seccomp_data(GENERIC_GETPID, AUDIT_ARCH_X86_64, 0)
                ),
                killed,
            );
        }
    }

    #[test]
    fn seccomp_dimensions_are_optional_and_union_without_overreach() {
        const SOCKET: i64 = 41;
        const IO_URING_SETUP: i64 = 425;
        const IO_URING_ENTER: i64 = 426;
        const IO_URING_REGISTER: i64 = 427;
        const KEYCTL: i64 = 250;
        const ADD_KEY: i64 = 248;
        const REQUEST_KEY: i64 = 249;
        const GETPID: u32 = 39;
        let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
        let allowed = u32::from(SeccompAction::Allow);
        let killed = u32::from(SeccompAction::KillProcess);

        assert!(
            build_seccomp(false, IpEgress::Denied, false, false, false)
                .unwrap()
                .is_none()
        );
        let build = |network, keyring, permit_join| {
            build_seccomp_for(
                TargetArch::x86_64,
                network,
                IpEgress::Denied,
                keyring,
                permit_join,
                false,
                SandboxSyscalls {
                    socket: SOCKET,
                    io_uring_setup: IO_URING_SETUP,
                    io_uring_enter: IO_URING_ENTER,
                    io_uring_register: IO_URING_REGISTER,
                    keyctl: KEYCTL,
                    add_key: ADD_KEY,
                    request_key: REQUEST_KEY,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        // Single-level filters (permit_keyring_join=false): the keyring dimension keeps
        // the strict deny-all keyctl of the pre-nesting filter.
        let network = build(true, false, false);
        let keyring = build(false, true, false);
        let union = build(true, true, false);

        for syscall in [KEYCTL as u32, ADD_KEY as u32, REQUEST_KEY as u32] {
            assert_eq!(
                evaluate_bpf(&network, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                allowed,
            );
            // keyctl(option 0) is a credential op, not the join — denied like the rest.
            assert_eq!(
                evaluate_bpf(&keyring, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                denied,
            );
            assert_eq!(
                evaluate_bpf(&union, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                denied,
            );
        }
        // A single-level keyring filter denies EVERY keyctl including the anonymous
        // session-keyring join — byte-identical to the pre-nesting filter.
        let join = libc::KEYCTL_JOIN_SESSION_KEYRING as u64;
        for program in [&keyring, &union] {
            assert_eq!(
                evaluate_bpf(
                    program,
                    &seccomp_data(KEYCTL as u32, AUDIT_ARCH_X86_64, join)
                ),
                denied,
                "single-level keyring filter must still deny the join (byte-identical)",
            );
        }
        // The nested-monitor carve-out (permit_keyring_join=true): the ANONYMOUS
        // session-keyring join is the one keyctl a nesting filter must permit (so a
        // nested monitor can isolate its child), while a NAMED join — which could
        // re-attach an ancestor keyring — and every other keyctl option stay denied.
        let keyring_nesting = build(false, true, true);
        let union_nesting = build(true, true, true);
        for program in [&keyring_nesting, &union_nesting] {
            assert_eq!(
                evaluate_bpf(
                    program,
                    &seccomp_data(KEYCTL as u32, AUDIT_ARCH_X86_64, join)
                ),
                allowed,
                "anonymous session-keyring join must be permitted for nested isolation",
            );
            assert_eq!(
                evaluate_bpf(
                    program,
                    &seccomp_data_arg1(KEYCTL as u32, AUDIT_ARCH_X86_64, join, 0x7fff_0000),
                ),
                denied,
                "a NAMED session-keyring join must stay denied even when nesting",
            );
            // A non-join keyctl option stays denied under the nesting filter too.
            assert_eq!(
                evaluate_bpf(program, &seccomp_data(KEYCTL as u32, AUDIT_ARCH_X86_64, 0)),
                denied,
                "a non-join keyctl option must stay denied under the nesting filter",
            );
        }
        for program in [&network, &keyring, &union] {
            assert_eq!(
                evaluate_bpf(program, &seccomp_data(GETPID, AUDIT_ARCH_X86_64, 0)),
                allowed,
            );
            assert_eq!(
                evaluate_bpf(
                    program,
                    &seccomp_data(GETPID | X32_SYSCALL_BIT, AUDIT_ARCH_X86_64, 0),
                ),
                denied,
            );
            assert_eq!(
                evaluate_bpf(program, &seccomp_data(GETPID, 0xc000_00b7, 0)),
                killed,
            );
        }
        assert_eq!(
            evaluate_bpf(
                &network,
                &seccomp_data(SOCKET as u32, AUDIT_ARCH_X86_64, libc::AF_UNIX as u64),
            ),
            denied,
        );
        assert_eq!(
            evaluate_bpf(
                &keyring,
                &seccomp_data(SOCKET as u32, AUDIT_ARCH_X86_64, libc::AF_UNIX as u64),
            ),
            allowed,
        );
        assert_eq!(
            evaluate_bpf(
                &union,
                &seccomp_data(SOCKET as u32, AUDIT_ARCH_X86_64, libc::AF_UNIX as u64),
            ),
            denied,
        );
    }

    #[test]
    fn permitted_ip_egress_lifts_only_the_two_ip_families() {
        // THE ASYMMETRY IS THE INVARIANT, and it holds for BOTH callers of `Permitted` — the
        // netns/bridge path and the Landlock build jail's per-package grant. Lifting the IP
        // families is the grant; lifting anything else would be an escape, because the rest of
        // the ceiling is not an egress question: AF_UNIX reaches host daemons through a
        // filesystem path nothing here scopes, AF_VSOCK is CID-addressed to the hypervisor, and
        // the buses are global. This test is what stops a future caller from widening the carve-
        // out along with it.
        const SOCKET: i64 = 41; // x86-64 socket(2)
        const IO_URING_SETUP: i64 = 425;
        const IO_URING_ENTER: i64 = 426;
        const IO_URING_REGISTER: i64 = 427;
        let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
        let allowed = u32::from(SeccompAction::Allow);
        let syscalls = || SandboxSyscalls {
            socket: SOCKET,
            io_uring_setup: IO_URING_SETUP,
            io_uring_enter: IO_URING_ENTER,
            io_uring_register: IO_URING_REGISTER,
            keyctl: 250,
            add_key: 248,
            request_key: 249,
            ..Default::default()
        };
        let permitted = build_seccomp_for(
            TargetArch::x86_64,
            true,
            IpEgress::Permitted,
            false,
            false,
            false,
            syscalls(),
        )
        .unwrap();
        let denied_ip = build_seccomp_for(
            TargetArch::x86_64,
            true,
            IpEgress::Denied,
            false,
            false,
            false,
            syscalls(),
        )
        .unwrap();
        let sock = |program: &BpfProgram, family: libc::c_int| {
            evaluate_bpf(
                program,
                &seccomp_data(SOCKET as u32, AUDIT_ARCH_X86_64, family as u64),
            )
        };
        // The two programs differ on exactly the IP families and nowhere else. This pairing is
        // the regression guard for the defect this replaced: `Permitted` and `Denied` used to
        // compile byte-identically on the build-jail path, so a granted package got nothing.
        for family in [libc::AF_INET, libc::AF_INET6] {
            assert_eq!(
                sock(&permitted, family),
                allowed,
                "a granted package must reach family {family}"
            );
            assert_eq!(
                sock(&denied_ip, family),
                denied,
                "an ungranted package must not reach family {family}"
            );
        }
        // Everything else stays denied under `Permitted` too.
        for family in [
            libc::AF_UNIX,
            libc::AF_VSOCK,
            libc::AF_PACKET,
            libc::AF_XDP,
            libc::AF_BLUETOOTH,
            libc::AF_RDS,
            libc::AF_CAN,
            libc::AF_TIPC,
            libc::AF_IB,
            libc::AF_NFC,
        ] {
            assert_eq!(
                sock(&permitted, family),
                denied,
                "permitted IP egress must still deny family {family}"
            );
        }
        // Every io_uring entry point stays blocked so a socket cannot be created off the family
        // filter. Blocking setup alone is not enough: an already-created ring is driven by
        // io_uring_enter, which is the path that reaches AF_VSOCK.
        for (name, syscall) in [
            ("io_uring_setup", IO_URING_SETUP),
            ("io_uring_enter", IO_URING_ENTER),
            ("io_uring_register", IO_URING_REGISTER),
        ] {
            assert_eq!(
                evaluate_bpf(
                    &permitted,
                    &seccomp_data(syscall as u32, AUDIT_ARCH_X86_64, 0)
                ),
                denied,
                "permitted IP egress must still block {name} (io_uring creates any-family sockets)"
            );
        }
    }

    /// The IR→ceiling mapping, the other half of the fix: ANY Allow — the catch-all `["*"]` the
    /// build jail now emits for a catalogued package, or a named host from a `nub sandbox`
    /// policy — must reach `Permitted`, and the deny-all an uncatalogued package compiles to must
    /// not. Pinned apart
    /// from the BPF assertions above because a correct filter reached through the wrong verdict is
    /// still a granted package with no network.
    #[test]
    fn the_ir_decides_the_ceiling_and_a_deny_all_axis_grants_nothing() {
        use crate::policy::{NetPolicy, NetRule, NetTarget};
        let deny_all = NetPolicy {
            enforce: true,
            default_effect: Effect::Deny,
            ..NetPolicy::default()
        };
        assert_eq!(ip_egress_for(&deny_all), IpEgress::Denied);
        let granted = NetPolicy {
            rules: vec![NetRule {
                target: NetTarget::Host("nodejs.org".to_string()),
                effect: Effect::Allow,
            }],
            ..deny_all.clone()
        };
        assert_eq!(ip_egress_for(&granted), IpEgress::Permitted);
        // The shape a catalogued package actually compiles to since per-host was dropped: a
        // catch-all naming no host. It must read as a grant exactly like the named host above,
        // or every catalogued package silently loses its egress on this backend.
        let coarse = NetPolicy {
            rules: vec![NetRule {
                target: NetTarget::Host("*".to_string()),
                effect: Effect::Allow,
            }],
            ..deny_all.clone()
        };
        assert_eq!(ip_egress_for(&coarse), IpEgress::Permitted);
        // A deny-ONLY rule list is still a deny-all base: an entry is not a grant.
        let deny_rule_only = NetPolicy {
            rules: vec![NetRule {
                target: NetTarget::Host("evil.test".to_string()),
                effect: Effect::Deny,
            }],
            ..deny_all
        };
        assert_eq!(ip_egress_for(&deny_rule_only), IpEgress::Denied);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn installed_network_seccomp_denies_raw_x32_syscalls() {
        unsafe fn denied(syscall: libc::c_long, arguments: [libc::c_long; 3]) -> bool {
            unsafe {
                *libc::__errno_location() = 0;
                libc::syscall(syscall, arguments[0], arguments[1], arguments[2]) == -1
                    && *libc::__errno_location() == libc::EPERM
            }
        }

        let program = build_seccomp(true, IpEgress::Denied, false, false, false)
            .unwrap()
            .unwrap();
        let child = unsafe { libc::fork() };
        assert!(
            child >= 0,
            "fork failed: {}",
            std::io::Error::last_os_error()
        );
        if child == 0 {
            if seccompiler::apply_filter(&program).is_err() {
                unsafe { libc::_exit(10) };
            }
            let x32 = libc::c_long::from(X32_SYSCALL_BIT);
            let checks = unsafe {
                [
                    denied(
                        libc::SYS_socket | x32,
                        [libc::AF_INET.into(), libc::SOCK_STREAM.into(), 0],
                    ),
                    denied(
                        libc::SYS_socket | x32,
                        [libc::AF_NETLINK.into(), libc::SOCK_RAW.into(), 0],
                    ),
                    denied(libc::SYS_io_uring_setup | x32, [1, 0, 0]),
                    denied(libc::SYS_getpid | x32, [0, 0, 0]),
                    denied(libc::c_long::from(u32::MAX), [0, 0, 0]),
                ]
            };
            if checks.iter().any(|denied| !denied) {
                unsafe { libc::_exit(11) };
            }
            if unsafe { libc::syscall(libc::SYS_getpid) } <= 0 {
                unsafe { libc::_exit(12) };
            }
            unsafe { libc::_exit(0) };
        }

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "raw x32 syscall child failed with wait status {status:#x}",
        );
    }

    /// Render a built option sequence for comparison. Descriptor numbers are
    /// allocation-dependent, so the value carried by an fd-valued option is
    /// normalised — everything else is compared byte-exactly.
    fn rendered_options(setup: &Command) -> Vec<String> {
        let mut rendered = Vec::new();
        let mut fd_valued = false;
        for arg in setup.get_args() {
            if std::mem::take(&mut fd_valued) {
                rendered.push("<fd>".to_string());
                continue;
            }
            fd_valued = matches!(arg.as_bytes(), b"--ro-bind-data" | b"--ro-bind-fd");
            rendered.push(arg.to_string_lossy().into_owned());
        }
        rendered
    }

    /// The golden argv for the confinement boundary. Bubblewrap applies these in
    /// order, so this pins ORDER as well as membership: dropping `--new-session`
    /// (the TIOCSTI defence) or `--cap-drop ALL`, or emitting a mask before the
    /// grant it must override, fails here rather than in production.
    ///
    /// Pinning the sequence EXACTLY is what makes it an injection assertion too. A
    /// single extra fail-open token turns a deny mask — which is a bind mount — into
    /// a silently skipped one: measured on bubblewrap 0.11.0, `--bind` over a missing
    /// source refuses the launch while `--bind-try` proceeds and leaves the masked
    /// file readable. (Upstream's global `--not-a-security-boundary`, which sets
    /// BIND_FAIL_OPEN for every bind, is unreleased as of 0.11.2 — an exact-sequence
    /// assertion covers it and anything else without enumerating flags.)
    #[test]
    fn confinement_options_pin_the_namespace_and_mount_boundary() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".env"), "SECRET").unwrap();
        let secrets = project.join("secrets");
        fs::create_dir_all(&secrets).unwrap();

        let policy = policy(
            root.path(),
            json!({
                "fs": {
                    "/**": "r",
                    "./": "rw",
                    secrets.display().to_string(): false,
                },
                "net": false,
            }),
        );
        let mount_plan = linux_grants::compile_mount_plan(&policy).unwrap();
        let masks = collect_masks(&policy, std::slice::from_ref(&project)).unwrap();
        assert_eq!(root_view(&policy), RootView::ReadOnly);

        let mut setup = Command::new("");
        let sources = append_confinement_options(
            &mut setup,
            &policy,
            RootView::ReadOnly,
            Path::new("/bin/true"),
            &mount_plan,
            &masks,
            None,
            None,
            None,
            &|command| {
                command.args(["--tmpfs", "/dev/.nub-sandbox/support"]);
            },
        )
        .unwrap();

        let project = project.display().to_string();
        assert_eq!(
            rendered_options(&setup),
            vec![
                "--die-with-parent",
                "--new-session",
                "--unshare-user",
                "--as-pid-1",
                "--cap-drop",
                "ALL",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--tmpfs",
                "/dev/.nub-sandbox/support",
                "--proc",
                "/proc",
                "--bind",
                &project,
                &project,
                // The two denies land in the order the policy authored them: the explicit
                // `secrets` deny, then the compiler's `.env` floor, which is appended after
                // every user entry. Each mask's `--remount-ro` is deferred past the whole
                // stream so a bind reopened inside one can still have its mountpoint made.
                "--perms",
                "000",
                "--tmpfs",
                &format!("{project}/secrets"),
                "--perms",
                "444",
                "--ro-bind-data",
                "<fd>",
                &format!("{project}/.env"),
                "--remount-ro",
                &format!("{project}/secrets"),
                "--remount-ro",
                "/dev/.nub-sandbox/support",
                "--unshare-net",
            ],
        );
        assert_eq!(sources.len(), 1, "one --ro-bind-data source is retained");
    }

    /// The two late-bound fd mounts are only distinguishable when BOTH are `Some`,
    /// so the golden above (both `None`) cannot tell them apart at all; this pins
    /// each to its own reserved destination. Also covers the private-tmp and
    /// minimal-root arms, whose remounts the read-only case never reaches.
    ///
    /// SCOPE: this catches a swap of the destinations WITHIN
    /// `append_confinement_options`. It cannot catch a transposition of the two
    /// same-typed `Option<&File>` arguments at the `configure_retained_outer` call
    /// site, because it invokes the callee directly — pinning that would need a real
    /// Bubblewrap and monitor image.
    #[test]
    fn late_bound_infrastructure_mounts_land_at_their_own_reserved_paths() {
        let root = tempdir().unwrap();
        let tmp_dir = root.path().join("private-tmp");
        fs::create_dir_all(&tmp_dir).unwrap();
        let policy = policy(root.path(), json!({"fs": {"$tmp": true}, "net": false}));

        let mut setup = Command::new("");
        let ca = tempfile::tempfile().unwrap();
        let bridge = tempfile::tempfile().unwrap();
        append_confinement_options(
            &mut setup,
            &policy,
            RootView::Minimal,
            Path::new("/bin/true"),
            &[],
            &[],
            Some(&tmp_dir),
            Some(&ca),
            Some(&bridge),
            &|command| {
                command.args(["--tmpfs", "/dev/.nub-sandbox/support"]);
            },
        )
        .unwrap();

        // The RAW args, not `rendered_options`: that helper normalises every fd value to
        // a placeholder, which would erase the very distinction under test here.
        let raw: Vec<String> = setup
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = |needle: &str| {
            raw.iter()
                .position(|a| a == needle)
                .unwrap_or_else(|| panic!("{needle} missing from {raw:?}"))
        };
        // The two descriptors must be distinguishable or the assertions below hold
        // equally for a transposed pair and prove nothing.
        assert_ne!(ca.as_raw_fd(), bridge.as_raw_fd());

        // Each late-bound mount is identified by its DESTINATION; the fd immediately
        // before it is what a transposed argument pair would swap.
        let ca_at = at(crate::backend::linux_monitor::PRIVATE_CA_BUNDLE);
        assert_eq!(raw[ca_at - 1], ca.as_raw_fd().to_string());
        assert_eq!(raw[ca_at - 2], "--ro-bind-data");
        assert_eq!(raw[ca_at - 4..ca_at - 2], ["--perms", "444"]);
        let bridge_at = at(crate::backend::linux_net_bridge::PRIVATE_NET_ROOT);
        assert_eq!(raw[bridge_at - 1], bridge.as_raw_fd().to_string());
        assert_eq!(raw[bridge_at - 2], "--ro-bind-fd");
        assert!(
            ca_at < bridge_at,
            "the CA bundle is layered before the net bridge: {raw:?}"
        );
        // Private /tmp binds the host dir; the minimal root seals itself read-only last.
        assert_eq!(raw[at("/tmp") - 1], tmp_dir.display().to_string());
        assert_eq!(raw[at("/tmp") - 2], "--bind");
        assert_eq!(raw.last().unwrap(), "--unshare-net");
        assert_eq!(raw[raw.len() - 3..raw.len() - 1], ["--remount-ro", "/"]);
    }

    /// `--args` is a NUL-SEPARATED stream, so an argument carrying an interior NUL
    /// would be split by Bubblewrap into further options — an injection channel
    /// ordinary execve argv cannot have. The rejection is the invariant that makes
    /// the option sequence above the whole option sequence.
    #[test]
    fn bwrap_argument_file_is_nul_separated_and_refuses_an_embedded_nul() {
        let clean = [
            OsStr::new("--ro-bind"),
            OsStr::new("/etc"),
            OsStr::new("/etc"),
        ];
        let mut file = write_bwrap_arguments(clean.into_iter()).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"--ro-bind\0/etc\0/etc\0");

        let injected = OsString::from_vec(b"/work\0--bind\0/\0/".to_vec());
        let error = write_bwrap_arguments(
            [OsStr::new("--ro-bind"), &injected, OsStr::new("/work")].into_iter(),
        )
        .expect_err("an argument carrying an interior NUL must not reach --args");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn generous_read_uses_read_only_root() {
        let root = tempdir().unwrap();
        assert_eq!(
            root_view(&policy(root.path(), json!(true))),
            RootView::ReadOnly
        );
    }

    #[test]
    fn direct_dotenv_masks_are_empty_readable() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".env"), "SECRET").unwrap();
        fs::write(project.join("ok.txt"), "OK").unwrap();
        let p = policy(root.path(), json!({"fs": ["**", "./"]}));
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert_eq!(
            masks
                .iter()
                .map(|mask| (&mask.path, mask.kind, mask.directory))
                .collect::<Vec<_>>(),
            vec![(&project.join(".env"), MaskKind::EmptyReadable, false)]
        );
    }

    #[test]
    fn nested_secret_files_are_masked_at_any_depth_but_node_modules_is_skipped() {
        // The project subtree is bind-mounted read-only as one tree, so a NESTED `.env`
        // (`apps/web/.env`) and a nested `.npmrc` (`packages/api/.npmrc`) would be readable
        // in-jail if the snapshot only checked immediate children — the H2 leak. The
        // recursive walk masks them at any depth: `.env` empty-readable, `.npmrc` unreadable.
        // A dep-internal `node_modules/.env` is deliberately NOT masked (cost skip — it is
        // the dependency's own shipped file, not a user secret).
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("apps/web")).unwrap();
        fs::create_dir_all(project.join("packages/api")).unwrap();
        fs::create_dir_all(project.join("node_modules/dep")).unwrap();
        fs::write(project.join(".env"), "ROOT_SECRET").unwrap();
        fs::write(project.join(".npmrc"), "//r/:_authToken=root").unwrap();
        fs::write(project.join("apps/web/.env"), "NESTED_SECRET").unwrap();
        fs::write(project.join("packages/api/.npmrc"), "//r/:_authToken=t").unwrap();
        fs::write(project.join("node_modules/dep/.env"), "DEP_OWN").unwrap();
        fs::write(project.join("apps/web/index.js"), "ok").unwrap();

        let p = policy(root.path(), json!({"fs": ["**", "./"]}));
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();

        let mask_for = |rel: &str| {
            let abs = fs::canonicalize(project.join(rel)).unwrap();
            masks.iter().find(|m| m.path == abs).cloned()
        };
        // `.env` (root + nested) → empty-readable; `.npmrc` (root + nested) → unreadable.
        assert_eq!(
            mask_for(".env").map(|m| m.kind),
            Some(MaskKind::EmptyReadable),
            "root .env must be masked empty-readable"
        );
        assert_eq!(
            mask_for("apps/web/.env").map(|m| m.kind),
            Some(MaskKind::EmptyReadable),
            "nested .env must be masked empty-readable"
        );
        assert_eq!(
            mask_for(".npmrc").map(|m| m.kind),
            Some(MaskKind::Unreadable),
            "root .npmrc must be masked unreadable"
        );
        assert_eq!(
            mask_for("packages/api/.npmrc").map(|m| m.kind),
            Some(MaskKind::Unreadable),
            "nested .npmrc must be masked unreadable"
        );
        assert!(
            mask_for("node_modules/dep/.env").is_none(),
            "a dep-internal node_modules/.env is not masked (cost skip)"
        );
    }

    /// npm's own builtin config (`<node-root>/lib/node_modules/npm/npmrc`, no leading dot
    /// — the `node_modules/npm/npmrc` band in `ENV_DENY_LEAF_GLOBS`) sits inside a
    /// directory literally named `node_modules`. CONTROL: seeding the deny walk only at
    /// the package dir (today's `pm_engine::build_jail` behavior before the fix) never
    /// reaches it — the escape. FIX: seeding the walk directly AT (or below) the
    /// `node_modules` dir reaches it, because `DENY_WALK_SKIP_DIRS` only blocks
    /// *descending into* a child named `node_modules`, not enumerating a root that already
    /// is one.
    #[test]
    fn npm_builtin_npmrc_needs_its_own_node_modules_dir_as_a_search_root() {
        let root = tempdir().unwrap();
        let package_dir = root.path().join("project/node_modules/some-dep");
        let npm_dir = root.path().join("node/lib/node_modules/npm");
        fs::create_dir_all(&package_dir).unwrap();
        fs::create_dir_all(&npm_dir).unwrap();
        fs::write(npm_dir.join("npmrc"), "//registry/:_authToken=LEAKED").unwrap();

        let p = policy(root.path(), json!({"fs": ["**", "./"]}));

        let escaped = collect_masks(&p, std::slice::from_ref(&package_dir)).unwrap();
        assert!(
            escaped.is_empty(),
            "control: package-dir-only search roots must not reach npm's npmrc \
             (reproduces the pre-fix escape): {escaped:?}"
        );

        let fixed = collect_masks(&p, &[package_dir.clone(), npm_dir.clone()]).unwrap();
        let masked = fixed
            .iter()
            .find(|m| m.path == fs::canonicalize(npm_dir.join("npmrc")).unwrap());
        assert_eq!(
            masked.map(|m| m.kind),
            Some(MaskKind::Unreadable),
            "adding npm's own node_modules dir as a search root must mask its npmrc: {fixed:?}"
        );
    }

    #[test]
    fn inaccessible_network_socket_masks_its_narrow_parent() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let root = tempdir().unwrap();
        let runtime = root.path().join("podman");
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join("podman.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let original = fs::metadata(&runtime).unwrap().permissions();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o000)).unwrap();

        let mask = net_equivalent_socket_mask(&socket)
            .expect("an inaccessible socket must remain maskable")
            .expect("the locked socket needs a mask");

        fs::set_permissions(&runtime, original).unwrap();
        assert_eq!(mask.path, fs::canonicalize(&runtime).unwrap());
        assert_eq!(mask.kind, MaskKind::Unreadable);
        assert!(mask.directory);
    }

    #[test]
    fn exact_explicit_deny_is_unreadable() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let denied = project.join("policy.sandbox.json");
        fs::write(&denied, "SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["**", "./", format!("!{}", denied.display())]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert!(
            masks
                .iter()
                .any(|m| m.path == denied && m.kind == MaskKind::Unreadable)
        );
    }

    #[test]
    fn explicit_dotenv_deny_is_unreadable_not_an_empty_readable_default() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let denied = project.join(".env");
        fs::write(&denied, "SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["**", "./", format!("!{}", denied.display())]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].kind, MaskKind::Unreadable);
    }

    #[cfg(unix)]
    #[test]
    fn logical_dotenv_symlink_is_masked_and_alias_convergence_keeps_unreadable() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let target = project.join("shared-secret");
        fs::write(&target, "SECRET").unwrap();
        symlink(&target, project.join(".env")).unwrap();

        let default_policy = policy(root.path(), json!({"fs": ["**", "./"]}));
        let masks = collect_masks(&default_policy, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].path, fs::canonicalize(&target).unwrap());
        assert_eq!(masks[0].kind, MaskKind::EmptyReadable);

        let explicit = policy(
            root.path(),
            json!({"fs": ["**", "./", format!("!{}", target.display())]}),
        );
        let masks = collect_masks(&explicit, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].path, fs::canonicalize(&target).unwrap());
        assert_eq!(masks[0].kind, MaskKind::Unreadable);
    }

    #[test]
    fn denied_regular_file_with_hardlink_alias_fails_closed() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let denied = project.join("secret.txt");
        fs::write(&denied, "SECRET").unwrap();
        fs::hard_link(&denied, project.join("alias.txt")).unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["**", "./", format!("!{}", denied.display())]}),
        );

        let error = collect_masks(&p, std::slice::from_ref(&project)).unwrap_err();
        assert!(error.contains(&denied.display().to_string()), "{error}");
        assert!(error.contains("2 hard links"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn logical_dotenv_symlink_reports_hardlinked_resolved_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let target = project.join("shared-secret");
        fs::write(&target, "SECRET").unwrap();
        fs::hard_link(&target, project.join("shared-secret-alias")).unwrap();
        let logical = project.join(".env");
        symlink(&target, &logical).unwrap();
        let p = policy(root.path(), json!({"fs": ["**", "./"]}));

        let error = collect_masks(&p, std::slice::from_ref(&project)).unwrap_err();
        assert!(error.contains(&logical.display().to_string()), "{error}");
        assert!(error.contains(&target.display().to_string()), "{error}");
        assert!(error.contains("2 hard links"), "{error}");
    }

    #[test]
    fn unrelated_hardlinks_do_not_affect_a_single_link_deny() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let package_file = project.join("package-store-file");
        fs::write(&package_file, "PACKAGE").unwrap();
        fs::hard_link(&package_file, project.join("package-store-alias")).unwrap();
        let denied = project.join("secret.txt");
        fs::write(&denied, "SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["**", "./", format!("!{}", denied.display())]}),
        );

        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].path, denied);
    }

    #[test]
    fn keyring_hardening_requires_a_resolved_withheld_environment_value() {
        let mut policy = SandboxPolicy::default();
        assert!(!protects_ambient_credentials(&policy));
        policy.env.resolved = true;
        policy.env.enforce = true;
        assert!(!protects_ambient_credentials(&policy));
        policy.env.withheld.push("TOKEN".to_string());
        assert!(protects_ambient_credentials(&policy));

        let masks = keyring_procfs_masks().unwrap();
        assert_eq!(masks.len(), 2);
        assert!(masks.iter().all(|mask| {
            mask.kind == MaskKind::Unreadable
                && !mask.directory
                && matches!(mask.path.to_str(), Some("/proc/keys" | "/proc/key-users"))
        }));
    }

    #[test]
    fn deny_search_roots_are_strict_and_exact_absence_is_skipped() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let p = policy(root.path(), json!({"fs": ["**", "./"]}));
        let missing = project.join("missing-root");
        let error = collect_masks(&p, &[missing]).unwrap_err();
        assert!(error.contains("deny-search root"), "{error}");

        let exact_missing = project.join("absent-policy.json");
        let p = policy(
            root.path(),
            json!({"fs": [format!("!{}", exact_missing.display())]}),
        );
        assert!(collect_masks(&p, &[]).unwrap().is_empty());
        assert!(!exact_missing.exists());
    }

    #[test]
    fn mask_merge_is_stable_and_unreadable_wins_alias_convergence() {
        let path = PathBuf::from("/same");
        let masks = merge_masks(vec![
            Mask {
                path: path.clone(),
                kind: MaskKind::EmptyReadable,
                directory: false,
                order: 1,
            },
            Mask {
                path: PathBuf::from("/z"),
                kind: MaskKind::EmptyReadable,
                directory: false,
                order: 2,
            },
            Mask {
                path: path.clone(),
                kind: MaskKind::Unreadable,
                directory: false,
                order: 4,
            },
        ]);
        assert_eq!(masks[0].path, path);
        assert_eq!(masks[0].kind, MaskKind::Unreadable);
        assert_eq!(masks[1].path, PathBuf::from("/z"));
    }

    /// The interleaved shape `[allow parent, deny child, allow grandchild]`, pinned at the
    /// option level. Bubblewrap applies operations in argv order and makes its own
    /// mountpoints, so all three of these are required together and each was measured to
    /// break the reopen on its own: the deny must sit BETWEEN the two binds, its
    /// `--remount-ro` must come after the nested bind (an immediate one makes the tmpfs
    /// read-only and `ensure_dir` fails EROFS), and its perms must be 111 rather than 000
    /// (000 removes traversal, and every launch drops CAP_DAC_READ_SEARCH, so the child
    /// could not reach the mount at all).
    #[test]
    fn a_reopened_child_is_bound_inside_its_denied_parent_and_stays_traversable() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        let denied = project.join("denied");
        let child = denied.join("child");
        fs::create_dir_all(&child).unwrap();
        let p = policy(
            root.path(),
            json!({"fs": [
                "**",
                project.to_string_lossy().to_string(),
                format!("!{}", denied.display()),
                child.to_string_lossy().to_string()
            ]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        let plan = linux_grants::compile_mount_plan(&p).unwrap();

        let mut setup = Command::new("");
        append_confinement_options(
            &mut setup,
            &p,
            RootView::ReadOnly,
            Path::new("/bin/true"),
            &plan,
            &masks,
            None,
            None,
            None,
            &|command| {
                command.args(["--tmpfs", "/dev/.nub-sandbox/support"]);
            },
        )
        .unwrap();

        let raw: Vec<String> = setup
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = |needle: &[&str]| {
            raw.windows(needle.len())
                .position(|w| w == needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {raw:?}"))
        };
        let denied_str = denied.display().to_string();
        let child_str = child.display().to_string();
        let parent_bind = at(&["--bind", &project.display().to_string()]);
        let mask = at(&["--perms", "111", "--tmpfs", &denied_str]);
        let child_bind = at(&["--bind", &child_str, &child_str]);
        let remount = at(&["--remount-ro", &denied_str]);
        assert!(
            parent_bind < mask && mask < child_bind && child_bind < remount,
            "the deny must be layered between the two binds and sealed only after the \
             reopen: parent={parent_bind} mask={mask} child={child_bind} remount={remount} \
             in {raw:?}"
        );
        assert!(
            !raw.windows(2).any(|w| w == ["000", "--tmpfs"]),
            "a mask holding a reopened child must not use traversal-removing 000: {raw:?}"
        );
    }

    /// Ordering must never let a bind be layered OVER a mask: a bind on the mask's own
    /// path or an ancestor of it replaces that subtree with the host's view, which would
    /// hand back the very file the deny hides. A mask authored before such a bind is
    /// clamped past it. The all-masks-last emitter got this for free; the ordered one has
    /// to state it.
    #[test]
    fn a_mask_is_clamped_past_any_bind_that_would_otherwise_cover_it() {
        let grant = |path: &str, rule_index: usize| MountGrant {
            path: PathBuf::from(path),
            access: MountAccess::ReadOnly,
            rule_index,
        };
        let mask = |path: &str, order: usize| Mask {
            path: PathBuf::from(path),
            kind: MaskKind::Unreadable,
            directory: false,
            order,
        };

        // The secret is authored FIRST and the covering bind second, so the raw key would
        // emit the mask and then bury it.
        let covered = mask("/p/.env", 1);
        let covering = grant("/p", 5);
        assert!(
            matches!(
                order_fs_operations(
                    std::slice::from_ref(&covering),
                    std::slice::from_ref(&covered)
                )
                .as_slice(),
                [FsOp::Bind(_), FsOp::Mask(_)]
            ),
            "a bind covering the mask must be emitted before it"
        );

        // Control: an UNRELATED bind must not drag the mask anywhere. Without this the
        // assertion above would also hold for a clamp that simply moved every mask last.
        assert!(
            matches!(
                order_fs_operations(&[grant("/other", 5)], std::slice::from_ref(&covered))
                    .as_slice(),
                [FsOp::Mask(_), FsOp::Bind(_)]
            ),
            "a bind on a disjoint path must leave the mask at its authored position"
        );

        // An infrastructure mask has no authored position and stays last regardless.
        let infra = Mask {
            order: INFRASTRUCTURE_ORDER,
            ..mask("/run/dbus", 0)
        };
        assert!(
            matches!(
                order_fs_operations(&[covering], &[infra]).as_slice(),
                [FsOp::Bind(_), FsOp::Mask(_)]
            ),
            "an infrastructure mask sorts after every policy bind"
        );
    }

    /// A deny with NOTHING reopened underneath keeps 000 — 111 is the narrow concession
    /// that a nested bind forces, not the new default.
    #[test]
    fn a_denied_directory_with_no_reopen_keeps_traversal_closed() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        let denied = project.join("denied");
        fs::create_dir_all(&denied).unwrap();
        let p = policy(
            root.path(),
            json!({"fs": [
                "**",
                project.to_string_lossy().to_string(),
                format!("!{}", denied.display()),
            ]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        let plan = linux_grants::compile_mount_plan(&p).unwrap();
        let ops = order_fs_operations(&plan, &masks);
        assert!(
            !reopened_below(&ops, &denied),
            "no grant lies inside {}: {ops:?}",
            denied.display()
        );
    }

    #[test]
    fn denied_subtree_with_exact_directory_allow_is_rejected() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        let denied = project.join("denied");
        fs::create_dir_all(&denied).unwrap();
        let p = policy(
            root.path(),
            json!({"fs": [
                "**",
                format!("!{}/**", denied.display()),
                denied.to_string_lossy().to_string()
            ]}),
        );
        let error = collect_masks(&p, std::slice::from_ref(&project)).unwrap_err();
        assert!(error.contains("exact directory allow"), "{error}");
    }

    #[test]
    fn direct_sandbox_config_glob_is_unreadable_at_any_depth() {
        // A `**/`-prefixed deny glob is depth-INDEPENDENT, so the recursive snapshot masks
        // every match — at the root AND nested — matching the policy intent. (Before the
        // recursive walk a nested match was silently left readable, an under-enforcement.)
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("nested")).unwrap();
        fs::write(project.join("tool.sandbox.json"), "SECRET").unwrap();
        fs::write(project.join("nested/ignored.sandbox.json"), "ALSO-SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["**", "./", "!**/*.sandbox.json"]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 2, "both the root and nested match are masked");
        for rel in ["tool.sandbox.json", "nested/ignored.sandbox.json"] {
            assert!(
                masks.iter().any(|m| m.path == project.join(rel)
                    && m.kind == MaskKind::Unreadable
                    && !m.directory),
                "{rel} must be masked unreadable"
            );
        }
    }

    #[test]
    fn nested_parent_glob_is_rejected_instead_of_under_scanned() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("nested")).unwrap();
        fs::write(project.join("nested/tool.sandbox.json"), "SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["**", "./", "!nested/*.sandbox.json"]}),
        );
        let error = collect_masks(&p, std::slice::from_ref(&project)).unwrap_err();
        assert!(error.contains("cannot be enforced"), "{error}");
    }

    #[test]
    fn mountinfo_paths_decode_kernel_octal_escapes() {
        assert_eq!(
            unescape_mountinfo_path(r"/tmp/with\040space\134slash").unwrap(),
            OsString::from("/tmp/with space\\slash")
        );
    }

    #[test]
    fn inherited_read_only_mount_caps_a_writable_request() {
        let mut plan = vec![linux_grants::MountGrant {
            path: PathBuf::from("/inherited/read-only"),
            access: MountAccess::ReadWrite,
            rule_index: 0,
        }];
        cap_inherited_read_only_with(&mut plan, |_| true);
        assert_eq!(plan[0].access, MountAccess::ReadOnly);
    }

    #[test]
    fn entry_resolution_uses_target_path_and_relative_entries_use_child_cwd() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("project");
        let bin = cwd.join("target-bin");
        fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("tool");
        fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

        let mut enforced = SandboxPolicy::default();
        enforced.env.enforce = true;
        enforced
            .env
            .constructed
            .insert("PATH".to_string(), "target-bin".to_string());
        assert_eq!(target_path(&enforced), Some(OsString::from("target-bin")));
        assert_eq!(
            resolve_program(OsStr::new("tool"), &cwd, target_path(&enforced).as_deref()),
            Some(fs::canonicalize(&tool).unwrap())
        );

        let mut missing = enforced.clone();
        missing.env.constructed.remove("PATH");
        assert_eq!(target_path(&missing), None);
        assert_eq!(resolve_program(OsStr::new("tool"), &cwd, None), None);

        let direct = cwd.join("direct");
        fs::write(&direct, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&direct, fs::Permissions::from_mode(0o755)).unwrap();
        let mut empty = enforced;
        empty
            .env
            .constructed
            .insert("PATH".to_string(), String::new());
        assert_eq!(target_path(&empty), Some(OsString::new()));
        assert_eq!(
            resolve_program(OsStr::new("direct"), &cwd, target_path(&empty).as_deref()),
            Some(fs::canonicalize(&direct).unwrap())
        );
    }

    #[test]
    fn entry_hidden_by_final_mask_or_tmp_view_is_rejected() {
        let entry = PathBuf::from("/tmp/project/tool");
        let mask = Mask {
            path: entry.clone(),
            kind: MaskKind::Unreadable,
            directory: false,
            order: 0,
        };
        assert!(validate_entry_visibility(&entry, TmpMode::Shared, &[mask], &[]).is_err());
        assert!(validate_entry_visibility(&entry, TmpMode::Deny, &[], &[]).is_err());
        let grant = linux_grants::MountGrant {
            path: PathBuf::from("/tmp/project"),
            access: MountAccess::ReadOnly,
            rule_index: 0,
        };
        assert!(validate_entry_visibility(&entry, TmpMode::Deny, &[], &[grant]).is_ok());
    }

    #[test]
    fn cwd_visibility_rejects_masks_and_tmp_but_accepts_a_real_submount() {
        let cwd = PathBuf::from("/tmp/project");
        let mask = Mask {
            path: cwd.clone(),
            kind: MaskKind::Unreadable,
            directory: true,
            order: 0,
        };
        assert!(
            validate_cwd_visibility(&cwd, RootView::ReadWrite, TmpMode::Shared, &[mask], &[])
                .is_err()
        );
        assert!(
            validate_cwd_visibility(&cwd, RootView::ReadWrite, TmpMode::Deny, &[], &[]).is_err()
        );
        let grant = linux_grants::MountGrant {
            path: PathBuf::from("/tmp/project"),
            access: MountAccess::ReadOnly,
            rule_index: 0,
        };
        assert!(
            validate_cwd_visibility(&cwd, RootView::Minimal, TmpMode::Private, &[], &[grant])
                .is_ok()
        );
        assert!(
            validate_cwd_visibility(
                Path::new("/workspace"),
                RootView::Minimal,
                TmpMode::Shared,
                &[],
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn process_inputs_reject_nuls_before_launch_setup() {
        let valid = CommandSpec::new("/bin/true").arg("ok").cwd("/");
        assert!(validate_process_inputs(&valid).is_ok());

        let bad_program = CommandSpec::new(OsString::from_vec(b"bad\0program".to_vec()));
        assert!(validate_process_inputs(&bad_program).is_err());

        let bad_arg =
            CommandSpec::new("/bin/true").arg(OsString::from_vec(b"bad\0argument".to_vec()));
        assert!(validate_process_inputs(&bad_arg).is_err());

        let bad_cwd = CommandSpec::new("/bin/true")
            .cwd(PathBuf::from(OsString::from_vec(b"/bad\0cwd".to_vec())));
        assert!(validate_process_inputs(&bad_cwd).is_err());
    }

    #[test]
    fn old_kernel_fd_fallback_marks_sparse_high_descriptors_cloexec() {
        let secret = tempfile::tempfile().unwrap();
        let secret_fd = secret.as_raw_fd();
        let mut initial_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut initial_limit) },
            0
        );
        let high_floor = initial_limit
            .rlim_cur
            .min(initial_limit.rlim_max)
            .saturating_sub(1)
            .min(4096) as i32;
        assert!(
            high_floor > 64,
            "test needs a descriptor above its child limit"
        );
        let high_fd = unsafe { libc::fcntl(secret_fd, libc::F_DUPFD_CLOEXEC, high_floor) };
        assert!(high_fd >= high_floor);
        assert_eq!(unsafe { libc::fcntl(secret_fd, libc::F_SETFD, 0) }, 0);
        assert_eq!(unsafe { libc::fcntl(high_fd, libc::F_SETFD, 0) }, 0);
        let high = unsafe { File::from_raw_fd(high_fd) };
        let script = format!(
            "test ! -e /proc/self/fd/{secret_fd} && test ! -e /proc/self/fd/{high_fd} && test ! -e /proc/self/fd/3"
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 64,
                    rlim_max: 64,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                cloexec_open_fds_from_proc()
            });
        }
        assert!(command.status().unwrap().success());
        drop(high);
    }

    #[test]
    fn bundled_candidate_executes_an_immutable_verified_snapshot() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("bwrap");
        let original = b"verified bubblewrap bytes";
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let source = File::open(&path).unwrap();
        let digest = format!("{:x}", Sha256::digest(original));
        let snapshot = executable_snapshot_with_digest(&source, &path, &digest).unwrap();

        // A retained descriptor to an ordinary inode observes in-place writes. The
        // executable authority must instead remain the sealed bytes we admitted.
        fs::write(&path, b"attacker replaced the opened source in place").unwrap();
        let mut observed = vec![0; original.len()];
        snapshot.read_exact_at(&mut observed, 0).unwrap();
        assert_eq!(observed, original);
        assert_eq!(snapshot.metadata().unwrap().len(), original.len() as u64);
        let seals = unsafe { libc::fcntl(snapshot.as_raw_fd(), libc::F_GET_SEALS) };
        assert_eq!(
            seals & REQUIRED_EXECUTABLE_SNAPSHOT_SEALS,
            REQUIRED_EXECUTABLE_SNAPSHOT_SEALS
        );
        assert!(snapshot.as_raw_fd() >= FIRST_LAUNCH_DATA_FD);
    }

    #[test]
    fn bundled_candidate_rejects_bytes_that_do_not_match_the_build() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("bwrap");
        fs::write(&path, b"wrong bundle").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let source = File::open(&path).unwrap();
        let expected = format!("{:x}", Sha256::digest(b"expected bundle"));

        let error = executable_snapshot_with_digest(&source, &path, &expected).unwrap_err();
        assert!(error.contains("digest mismatch"), "{error}");
    }

    #[test]
    fn system_candidate_trust_is_decided_from_the_opened_inode() {
        let trusted = open_pinned_bwrap_candidate(Path::new("/bin/true"), BubblewrapOrigin::System)
            .expect("the platform /bin/true should be root-owned and non-writable");
        assert!(trusted.program().starts_with("/proc/self/fd/"));

        let directory = tempdir().unwrap();
        let writable = directory.path().join("bwrap");
        fs::write(&writable, b"not trusted").unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let error = open_pinned_bwrap_candidate(&writable, BubblewrapOrigin::System)
            .err()
            .expect("group/other-writable helper was accepted");
        assert!(error.contains("root-owned"), "{error}");
    }

    #[test]
    fn system_candidate_requires_exact_root_owner() {
        assert!(trusted_system_candidate_metadata(0, 0o755));
        assert!(!trusted_system_candidate_metadata(12_345, 0o755));
        assert!(!trusted_system_candidate_metadata(u32::MAX, 0o755));
    }

    #[test]
    fn helper_owner_gate_tolerates_the_nested_overflow_owner() {
        let overflow = overflow_uid();
        // Host view (root-owned) and the nested view (host-root unmapped → overflow)
        // both pass the owner gate; the digest verification is the real guarantee.
        assert!(trusted_helper_metadata(0, 0o750));
        assert!(trusted_helper_metadata(overflow, 0o750));
        // Group/other write is still forbidden regardless of the reported owner, and an
        // arbitrary mapped owner (neither root nor overflow) is still rejected — a
        // system candidate stays strictly root-only.
        assert!(!trusted_helper_metadata(0, 0o770));
        assert!(!trusted_helper_metadata(overflow, 0o757));
        assert!(!trusted_helper_metadata(1000, 0o750));
        assert!(!trusted_system_candidate_metadata(overflow, 0o750));
    }

    #[test]
    fn candidate_open_rejects_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target");
        let alias = directory.path().join("bwrap");
        fs::write(&target, b"candidate").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &alias).unwrap();
        let error = open_pinned_bwrap_candidate(&alias, BubblewrapOrigin::Bundled)
            .err()
            .expect("symlink candidate was accepted");
        assert!(error.contains("opening candidate"), "{error}");
    }

    #[test]
    fn reserved_monitor_runtime_rejects_every_authored_overlap() {
        let root = Path::new(crate::backend::linux_monitor::PRIVATE_RUNTIME_ROOT);
        let entry = Path::new("/bin/true");
        assert!(validate_reserved_runtime_view(Path::new("/"), entry, &[], &[]).is_ok());
        assert!(validate_reserved_runtime_view(root, entry, &[], &[]).is_err());
        let grant = MountGrant {
            path: PathBuf::from("/dev"),
            access: MountAccess::ReadOnly,
            rule_index: 0,
        };
        assert!(validate_reserved_runtime_view(Path::new("/"), entry, &[grant], &[]).is_err());
        let mask = Mask {
            path: root.join("lib"),
            kind: MaskKind::Unreadable,
            directory: true,
            order: 0,
        };
        assert!(validate_reserved_runtime_view(Path::new("/"), entry, &[], &[mask]).is_err());
    }

    #[test]
    fn bwrap_failure_diagnostics_are_host_specific_without_admin_advice() {
        let message =
            classify_bwrap_failures(&["candidate: unknown option --info-fd".into()], false);
        assert!(message.contains("required stock options"), "{message}");
        for banned in ["sudo", "sysctl", "disable AppArmor", "apparmor_parser"] {
            assert!(!message.contains(banned), "{message}");
        }
    }

    #[test]
    fn apparmor_hint_covers_the_loopback_denial_a_restricted_host_actually_emits() {
        // The observed stderr on a locked-down Ubuntu 24.04 host: `--unshare-all`
        // fails bringing up the netns loopback, BEFORE any uid-map step, so it
        // names neither "permission denied" nor "uid map". Matching only those two
        // left the setup hint dead on the exact case it was written for.
        for detail in [
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted",
            "bwrap: setting up uid map: Permission denied",
        ] {
            let message = classify_bwrap_failures_under(&[detail.to_string()], false, true, false);
            assert!(
                message.contains(crate::backend::linux_setup::SETUP_COMMAND),
                "a restricted host must get the setup hint, not the generic message: {message}"
            );
        }
        // Same failure on an UNRESTRICTED host is not a setup problem, so it must
        // not claim one.
        let message = classify_bwrap_failures_under(
            &["bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted".to_string()],
            false,
            false,
            false,
        );
        assert!(
            !message.contains(crate::backend::linux_setup::SETUP_COMMAND),
            "{message}"
        );
    }

    #[test]
    fn an_unreachable_bind_source_is_not_blamed_on_the_apparmor_setup() {
        // The verbatim stderr of `sudo nub install` with nub under a 0750 $HOME. It is an
        // EACCES, so the old bare "permission denied" match routed it to the AppArmor arm
        // and told the user to run a setup that cannot possibly fix it.
        let detail = "bwrap: Can't find source path /proc/self/fd/204: Permission denied";
        for restricted in [true, false] {
            let message =
                classify_bwrap_failures_under(&[detail.to_string()], false, restricted, false);
            assert!(
                !message.contains(crate::backend::linux_setup::SETUP_COMMAND),
                "restricted={restricted}: {message}"
            );
            assert!(
                message.contains("resolves the mount by path"),
                "restricted={restricted}: {message}"
            );
        }
    }

    #[test]
    fn a_plain_eacces_is_not_read_as_a_namespace_denial() {
        // A bind-mount EACCES carries the same errno as a userns denial and nothing else. It
        // used to match, so an unreadable path on a restricted host told the caller to run the
        // setup — a remedy that cannot fix a file mode. The generic message is the honest one.
        for detail in [
            "bwrap: Can't create file at /newroot/etc/hosts: Permission denied",
            "bwrap: Can't read /var/lib/secret: Operation not permitted",
        ] {
            let message = classify_bwrap_failures_under(&[detail.to_string()], false, true, false);
            assert!(
                !message.contains(crate::backend::linux_setup::SETUP_COMMAND),
                "a plain EACCES must not be blamed on the AppArmor userns grant: {message}"
            );
        }
    }

    #[test]
    fn an_errno_alone_is_not_read_as_a_namespace_denial() {
        // Bubblewrap always names the step it failed at, so the operation is the signal.
        for named in [
            "bwrap: setting up uid map: Permission denied",
            "bwrap: setting up gid map: Permission denied",
            "bwrap: No permissions to create a new namespace",
            "bwrap: Creating new namespace failed: Operation not permitted",
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted",
            // The second-level namespace `--dev` forces for every non-root launch.
            "bwrap: unshare user ns: Operation not permitted",
            "bwrap: unshare pid ns: Operation not permitted",
            "bwrap: error writing to setgroups: Permission denied",
        ] {
            assert!(is_namespace_denial(&named.to_ascii_lowercase()), "{named}");
        }
        for unnamed in [
            "bwrap: Can't find source path /proc/self/fd/204: Permission denied",
            "bwrap: Can't create file at /x: Operation not permitted",
        ] {
            assert!(
                !is_namespace_denial(&unnamed.to_ascii_lowercase()),
                "{unnamed}"
            );
        }
    }

    #[test]
    fn every_bubblewrap_userns_denial_still_reaches_the_setup_hint() {
        // Enumerated from bubblewrap's own `die`/`die_with_error` sites on the namespace and
        // credential paths. Narrowing the match to named operations must not drop any of them —
        // RTM_NEWLINK and the setgroups write have no "namespace" in their text and were the
        // two the first narrowing lost.
        for detail in [
            "bwrap: setting up uid map: Permission denied",
            "bwrap: setting up gid map in child: Permission denied",
            "bwrap: Creating new namespace failed: Operation not permitted",
            "bwrap: No permissions to creating new namespace, likely because the kernel does \
             not allow non-privileged user namespaces",
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted",
            "bwrap: loopback: Failed RTM_NEWLINK: Operation not permitted",
            "bwrap: error writing to setgroups: Permission denied",
            "bwrap: sysctl user.max_user_namespaces = 1",
        ] {
            let message = classify_bwrap_failures_under(&[detail.to_string()], false, true, false);
            assert!(
                message.contains(crate::backend::linux_setup::SETUP_COMMAND),
                "a userns denial must reach the setup hint: {detail}\n{message}"
            );
        }
    }

    #[test]
    fn a_container_is_blamed_before_the_host_apparmor_restriction() {
        // Both conditions hold at once inside a container on a restricted host, and
        // the container is the one the caller can act on: host setup run inside a
        // container fixes nothing, so naming it sends the reader after AppArmor for
        // a container-policy problem.
        for nesting in [false, true] {
            let message = classify_bwrap_failures_under(
                &["bwrap: setting up uid map: Permission denied".to_string()],
                nesting,
                true,
                true,
            );
            assert!(
                message.contains("container policy"),
                "nesting={nesting}: {message}"
            );
            assert!(
                !message.contains(crate::backend::linux_setup::SETUP_COMMAND),
                "nesting={nesting}: {message}"
            );
        }
    }

    #[test]
    fn dedicated_helper_rejects_a_non_root_owner() {
        // A test-user-owned file can never be the root-owned helper. Ownership is
        // checked from the OPENED inode, before the digest, so the error names it.
        let directory = tempdir().unwrap();
        let path = directory.path().join("nub-bwrap");
        fs::write(&path, b"not the helper").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        let error = open_pinned_bwrap_candidate(&path, BubblewrapOrigin::DedicatedHelper)
            .err()
            .expect("a non-root-owned helper was admitted");
        assert!(error.contains("root-owned"), "{error}");
    }

    #[test]
    fn dedicated_helper_digest_admits_matching_bytes_and_rejects_tampering() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nub-bwrap");
        let packaged = b"the exact packaged bubblewrap bytes";
        fs::write(&path, packaged).unwrap();
        let source = File::open(&path).unwrap();
        let digest = format!("{:x}", Sha256::digest(packaged));
        verify_pinned_helper_digest_against(&source, &path, &digest)
            .expect("packaged bytes must verify against their own digest");

        // A single flipped byte fails closed against the pinned digest.
        let tampered = File::open({
            let other = directory.path().join("tampered");
            fs::write(&other, b"the exact packaged bubblewrap byteS").unwrap();
            other
        })
        .unwrap();
        let error = verify_pinned_helper_digest_against(&tampered, &path, &digest)
            .expect_err("tampered bytes were accepted");
        assert!(error.contains("digest"), "{error}");
    }

    #[test]
    fn dedicated_helper_digest_rejects_a_malformed_pin() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nub-bwrap");
        fs::write(&path, b"bytes").unwrap();
        let source = File::open(&path).unwrap();
        let error = verify_pinned_helper_digest_against(&source, &path, "not-64-hex")
            .expect_err("a malformed digest pin was accepted");
        assert!(error.contains("malformed"), "{error}");
    }

    #[test]
    fn nesting_failure_classifies_group_access_precisely() {
        let failures = vec![format!(
            "{DEDICATED_HELPER_ACCESS_TAG}: Permission denied (os error 13)"
        )];
        let message = classify_bwrap_failures(&failures, true);
        assert!(message.contains("nub-sandbox group"), "{message}");
        assert!(message.contains("fresh login"), "{message}");
        assert_no_privileged_advice(&message);
    }

    #[test]
    fn nesting_failure_classifies_missing_helper() {
        // C1 reverses the old "no remediation command" posture for the NOT-SET-UP case: a
        // missing helper is a setup problem, so the error names the exact setup command.
        let failures =
            vec!["opening candidate: No such file or directory (os error 2)".to_string()];
        let message = classify_bwrap_failures(&failures, true);
        assert!(message.contains("not installed"), "{message}");
        assert!(message.contains(DEDICATED_HELPER_PATH), "{message}");
        assert!(
            message.contains(crate::backend::linux_setup::SETUP_COMMAND),
            "{message}"
        );
    }

    #[test]
    fn apparmor_setup_hint_names_the_setup_command() {
        // The C1 remedy text every not-set-up fail-closed path routes through.
        let hint = apparmor_setup_hint();
        assert!(
            hint.contains(crate::backend::linux_setup::SETUP_COMMAND),
            "{hint}"
        );
        assert!(hint.contains(DEDICATED_HELPER_PATH), "{hint}");
    }

    #[test]
    fn the_hint_names_the_no_prompt_repair_only_where_sudo_needs_no_password() {
        // A machine that can repair itself non-interactively (a GitHub runner) should be told
        // so with the exact command; one that would prompt must not imply otherwise. nub only
        // REPORTS this — it never runs sudo on its own initiative.
        let passwordless = apparmor_setup_hint_with(true);
        assert!(
            passwordless.contains("Passwordless sudo works here"),
            "{passwordless}"
        );
        assert!(
            passwordless.contains(crate::backend::linux_setup::SETUP_COMMAND_ALL_USERS),
            "{passwordless}"
        );
        assert!(
            !apparmor_setup_hint_with(false).contains("Passwordless sudo works here"),
            "an interactive host must not claim a prompt-free repair"
        );
    }

    #[test]
    fn nesting_failure_classifies_integrity() {
        for detail in [
            "the dedicated helper does not match the packaged Bubblewrap build (digest mismatch): x",
            "dedicated nesting helper is not root-owned and protected from group/other writes (uid=1000, mode=755)",
        ] {
            let message = classify_bwrap_failures(&[detail.to_string()], true);
            assert!(message.contains("integrity check"), "{message}: {detail}");
            assert_no_privileged_advice(&message);
        }
    }

    fn assert_no_privileged_advice(message: &str) {
        for banned in [
            "sudo",
            "sysctl",
            "disable AppArmor",
            "apparmor_parser",
            "groupadd",
        ] {
            assert!(!message.contains(banned), "leaked admin command: {message}");
        }
    }

    #[test]
    fn nesting_inventory_admits_only_the_dedicated_helper() {
        // The require-nesting inventory considers exactly one path on every host,
        // so both outcomes are assertable: an unprovisioned host (every CI runner)
        // must yield no candidate and record the helper's rejection rather than
        // widening to a stock bwrap, and a provisioned one must admit only the
        // helper itself.
        let inventory = open_bwrap_candidate_inventory(true);
        if inventory.candidates.is_empty() {
            assert!(
                !inventory.failures.is_empty(),
                "an empty require-nesting inventory recorded no reason for rejecting {DEDICATED_HELPER_PATH}"
            );
            for failure in &inventory.failures {
                assert!(
                    failure.starts_with(DEDICATED_HELPER_PATH),
                    "require-nesting consulted a non-helper candidate: {failure}"
                );
            }
        } else {
            for candidate in &inventory.candidates {
                assert_eq!(
                    candidate.source_path,
                    Path::new(DEDICATED_HELPER_PATH),
                    "a non-helper candidate entered the nesting inventory"
                );
            }
        }
    }

    #[test]
    fn probe_diagnostic_drain_completes_on_eof_and_gives_up_on_a_held_pipe() {
        fn piped_child(script: &str) -> std::process::Child {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn a probe stand-in with piped stderr")
        }

        let mut writer = piped_child("printf boom >&2");
        writer.wait().expect("await the exited stand-in");
        let collected = drain_probe_diagnostic(&mut writer.stderr.take().expect("piped stderr"));
        assert_eq!(
            String::from_utf8_lossy(&collected),
            "boom",
            "the drain dropped diagnostic bytes already buffered in a closed pipe"
        );

        // `exec sleep` inherits the write end, so waiting for EOF here would block
        // apply() for the descendant's lifetime — the defect this drain bounds.
        let mut holder = piped_child("exec sleep 300");
        let mut held_stderr = holder.stderr.take().expect("piped stderr");
        let started = Instant::now();
        drain_probe_diagnostic(&mut held_stderr);
        let elapsed = started.elapsed();
        let _ = holder.kill();
        let _ = holder.wait();
        assert!(
            elapsed < Duration::from_secs(5),
            "the drain blocked {elapsed:?} on a pipe a live descendant still holds open"
        );
    }
}
