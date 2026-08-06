//! Landlock filesystem confinement — the UNPRIVILEGED Linux build-jail mechanism.
//!
//! Bubblewrap cannot confine the build jail on a large share of real hosts: it passes
//! `--unshare-user`, and an unprivileged user namespace is denied by default on Ubuntu
//! 23.10–25.04 (`apparmor_restrict_unprivileged_userns=1`) and is impossible inside a
//! container (no `CAP_SYS_ADMIN` — even root cannot create one). There the jail fails
//! closed, so `nub install` breaks on any dependency carrying a lifecycle script.
//! `landlock_restrict_self` needs no namespace and no privilege at all, which is the
//! whole reason this backend exists.
//!
//! WHY THIS IS EXPRESSIBLE AT ALL: Landlock rules UNION — there is no deny primitive at
//! any ABI, so "deny inside allow" cannot be written. The build jail is a PURE ALLOWLIST
//! that emits zero deny rules (`preset::enforce_pure_allowlist`), so the objection that
//! once disqualified Landlock does not bind here. It still binds `nub sandbox`, which is
//! why that product keeps bubblewrap and its escalation path.
//!
//! The rule set is derived from the SAME [`compile_mount_plan`] the bubblewrap backend
//! consumes, so the two mechanisms cannot drift on which paths a policy grants.

use super::linux_grants::{MountAccess, MountGrant, compile_mount_plan};
use crate::policy::SandboxPolicy;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

// `landlock_access_fs_t` bits, in ABI order. Kept as raw constants rather than pulled
// from the `landlock` crate so the READ→EXECUTE coupling below is stated and tested
// HERE, where a future reader looking at a broken native build will land.
const ACCESS_EXECUTE: u64 = 1 << 0;
const ACCESS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_READ_FILE: u64 = 1 << 2;
const ACCESS_READ_DIR: u64 = 1 << 3;
const ACCESS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_MAKE_REG: u64 = 1 << 8;
const ACCESS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_MAKE_SYM: u64 = 1 << 12;
const ACCESS_REFER: u64 = 1 << 13; // ABI 2
const ACCESS_TRUNCATE: u64 = 1 << 14; // ABI 3
const ACCESS_IOCTL_DEV: u64 = 1 << 15; // ABI 5

/// The rights that are meaningful on a NON-directory. Landlock rejects a rule with
/// `EINVAL` when a directory-only right is attached to a file `parent_fd`, and the jail
/// grants individual files (`<project>/package.json`, `/etc/resolv.conf`, the interpreter
/// binary), so every access set is masked through this before `add_rule`.
const FILE_ONLY_RIGHTS: u64 =
    ACCESS_EXECUTE | ACCESS_WRITE_FILE | ACCESS_READ_FILE | ACCESS_TRUNCATE | ACCESS_IOCTL_DEV;

/// The kernel's minimum Landlock ABI for filesystem rules. Below this the mechanism does
/// not exist and the caller must fall back or refuse.
pub(crate) const MIN_FS_ABI: u32 = 1;

/// The system closure a confined build needs to be able to EXECUTE and read: the loader,
/// libc, the compiler toolchain, and the resolver/trust material.
///
/// Deliberately shares [`super::linux::ESSENTIAL_READ_PATHS`] rather than restating it —
/// but the LEAF shape of that list is load-bearing here in a way it is not under
/// bubblewrap. `/etc/resolv.conf` is a symlink to `/run/systemd/resolve/stub-resolv.conf`
/// on every systemd host; bubblewrap resolves it at bind time, while Landlock evaluates
/// the RESOLVED path against the ruleset, so a grant on the `/etc` DIRECTORY leaves the
/// real file outside every rule and silently kills DNS for the whole network-allowed tier.
/// Opening each leaf with `O_PATH` (which follows symlinks) keys the rule on the target
/// inode instead, which is why the list must name files, not their parent directory.
fn system_read_paths() -> impl Iterator<Item = &'static str> {
    super::linux::ESSENTIAL_READ_PATHS.iter().copied()
}

/// The global `/proc` files a build toolchain reads — CPU count for `make -j`, memory for
/// a linker's heuristics, `/proc/sys` for limits.
///
/// Deliberately NOT the whole tree, which is what bubblewrap's fresh `--proc` mount
/// effectively gave. Landlock has no PID namespace, so a grant on `/proc` reaches every
/// same-uid process's per-pid directory.
///
/// ⛔ THE EXPOSURE THAT JUSTIFIES THIS IS HALF WHAT THIS COMMENT USED TO CLAIM, and the
/// correction is MEASURED — kernel 6.17, ABI v7, controls both directions, written up in
/// `wiki/research/landlock-proc-exposure.md`. It said a `/proc` grant would expose other
/// processes' `environ` AND `cmdline`. **`environ` is refused**, along with `maps`, `cwd`,
/// `exe`, `fdinfo` and `root`: they open through `ptrace_may_access`, and Landlock's own
/// ptrace hook denies that unless the reader's domain is an ancestor of the target's. A
/// confined script reads the environ of a process INSIDE its domain and is refused one
/// outside it — at `ptrace_scope` 0 as well as 1, so this is the confinement's own property
/// and does not depend on the distribution's yama default. **`cmdline` really is readable**,
/// as are `stat`, `status`, `comm`, `limits` and `mountinfo`.
///
/// So the surviving argument is narrower than "environment variables included": it is other
/// processes' command lines, which can carry a credential passed as an argv element. Granting
/// `READ_FILE` without `READ_DIR` additionally denies `readdir(/proc)`, so reaching one takes
/// a guessed pid. Whether that trade beats leaving the affected packages at `write:"disk"` —
/// which grants read and write on the real `~/.ssh` and `~/.npmrc` — is a product call about
/// security posture, and it is deliberately NOT made here. The list stays as it is until it is.
///
/// Per-process entries are unreachable either way, INCLUDING the child's own, and the reason
/// is deeper than the build order: a rule pins the INODE resolved when the ruleset is built,
/// so `/proc/self` names nub's own directory. Measured — a child is refused its own
/// `/proc/self/stat` while reading the parent's by explicit pid. Building the ruleset after
/// `fork` would therefore cover the direct child only, and the process that needs this is
/// routinely a grandchild.
const PROC_READ_PATHS: &[&str] = &[
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/stat",
    "/proc/uptime",
    "/proc/loadavg",
    "/proc/sys/vm/overcommit_memory",
    "/proc/sys/kernel/osrelease",
    "/proc/sys/kernel/ostype",
];

/// Character devices a build script legitimately needs. Granted read+write (never execute,
/// never `IOCTL_DEV`) because `/dev` as a whole is a reserved tree the mount planner refuses.
///
/// `/dev/tty` is deliberately ABSENT. It is the process's CONTROLLING terminal, and handing
/// it to a dependency's install script is the TIOCSTI injection vector the `setsid` above
/// exists to close — granting the node back would reopen it. Bubblewrap never exposed the
/// host's tty either: its `--dev` supplied a fresh devtmpfs.
const DEVICE_PATHS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/ptmx",
];

/// What a granted path may do. Mirrors [`MountAccess`] plus the device arm, which has no
/// bubblewrap analogue (bubblewrap gets device nodes from `--dev`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LandlockAccess {
    /// LIST the directory and open nothing in it — the IR's node-only read grant.
    ///
    /// Landlock's unit of grant is a path and its rights are inherited by everything
    /// beneath it, so a node cannot be separated from its subtree by PATH. It can by
    /// RIGHT: `READ_DIR` alone permits the `readdir` the grant exists for while
    /// withholding `READ_FILE`, so a file below stays unopenable unless a rule names it.
    ListDir,
    /// Read, list, and EXECUTE. See [`LandlockAccess::rights`] for why execute is here.
    ReadExecute,
    /// Every right the ABI handles.
    ReadWrite,
    /// A character device: read and write the node, never execute it.
    Device,
    /// EVERY right the ruleset handles — the catalog's full-disk tier, attached to `/`.
    ///
    /// The one arm that deliberately aliases [`handled_access_fs`] where every other arm
    /// enumerates. The enumeration exists so a right a future ABI adds is not granted by
    /// accident; here granting exactly what is handled is the WHOLE MEANING — a ruleset
    /// whose root grants every handled right restricts nothing, which is what "the whole
    /// filesystem, read and write" has to compile to. Enumerating instead would make the
    /// tier quietly narrower than the catalog says the moment an ABI grows: `ReadWrite`
    /// already withholds `MAKE_SOCK`/`MAKE_FIFO`/`MAKE_CHAR`/`MAKE_BLOCK`, so a script
    /// creating a unix socket would still fail under a grant that claims to be unconfined —
    /// a residual failure in the one tier whose reason for existing is having none.
    FullDisk,
}

impl LandlockAccess {
    /// The `landlock_access_fs_t` set this access grants at `abi`.
    ///
    /// Each arm ENUMERATES its rights rather than aliasing [`handled_access_fs`]. The two
    /// sets answer different questions — handled = "what this ruleset governs at all",
    /// granted = "what this path may do" — and collapsing them means every right a future
    /// ABI adds is granted automatically the moment it becomes handled. That is how the
    /// writable grant silently acquired `MAKE_CHAR`/`MAKE_BLOCK`, i.e. device-node creation,
    /// which bubblewrap denied outright via `--cap-drop ALL` under a user namespace. Node
    /// creation, socket and FIFO creation stay HANDLED (so they are denied everywhere) and
    /// GRANTED nowhere.
    ///
    /// PROVENANCE — the `ReadExecute` arm is why this function is spelled out rather than
    /// deferred to a helper: a bubblewrap `--ro-bind` grants EXECUTE for free, because a
    /// read-only mount still permits `execve`. Landlock does not — `LANDLOCK_ACCESS_FS_EXECUTE`
    /// is a separate right, and a read grant without it makes every binary under the path
    /// `EACCES` on exec. Rendering [`crate::policy::FsAccess::Read`] as read-WITHOUT-execute
    /// broke native addon builds outright during the prototype (the compiler, `sh`, and the
    /// interpreter all live under read grants), and is the single reason the bubblewrap
    /// corpus pass-rate does not transfer to this backend unchanged. Do not "tighten" this
    /// by dropping EXECUTE.
    fn rights(self, abi: u32) -> u64 {
        match self {
            // No EXECUTE and no READ_FILE: both are rights over the FILES under the path,
            // which is precisely what a node-only grant withholds.
            LandlockAccess::ListDir => ACCESS_READ_DIR,
            LandlockAccess::ReadExecute => {
                ACCESS_READ_FILE | ACCESS_READ_DIR | ACCESS_EXECUTE | ioctl_dev(abi)
            }
            LandlockAccess::ReadWrite => {
                ACCESS_READ_FILE
                    | ACCESS_READ_DIR
                    | ACCESS_EXECUTE
                    | ACCESS_WRITE_FILE
                    | ACCESS_REMOVE_DIR
                    | ACCESS_REMOVE_FILE
                    | ACCESS_MAKE_DIR
                    | ACCESS_MAKE_REG
                    | ACCESS_MAKE_SYM
                    | if abi >= 2 { ACCESS_REFER } else { 0 }
                    | if abi >= 3 { ACCESS_TRUNCATE } else { 0 }
                    | ioctl_dev(abi)
            }
            // No `IOCTL_DEV`: ABI 5+ is the first that can WITHHOLD terminal/device ioctls,
            // so granting it back is a strict loss against the one kernel that offers the
            // control. Ordinary read/write on these nodes needs no ioctl.
            LandlockAccess::Device => ACCESS_READ_FILE | ACCESS_WRITE_FILE,
            LandlockAccess::FullDisk => handled_access_fs(abi),
        }
    }
}

fn ioctl_dev(abi: u32) -> u64 {
    if abi >= 5 { ACCESS_IOCTL_DEV } else { 0 }
}

/// Every right this kernel's Landlock understands. A ruleset must declare the rights it
/// HANDLES; anything not handled stays entirely unrestricted, so under-declaring here
/// silently widens the jail.
fn handled_access_fs(abi: u32) -> u64 {
    let mut handled = ACCESS_EXECUTE
        | ACCESS_WRITE_FILE
        | ACCESS_READ_FILE
        | ACCESS_READ_DIR
        | ACCESS_REMOVE_DIR
        | ACCESS_REMOVE_FILE
        | ACCESS_MAKE_CHAR
        | ACCESS_MAKE_DIR
        | ACCESS_MAKE_REG
        | ACCESS_MAKE_SOCK
        | ACCESS_MAKE_FIFO
        | ACCESS_MAKE_BLOCK
        | ACCESS_MAKE_SYM;
    if abi >= 2 {
        handled |= ACCESS_REFER;
    }
    if abi >= 3 {
        handled |= ACCESS_TRUNCATE;
    }
    if abi >= 5 {
        handled |= ACCESS_IOCTL_DEV;
    }
    handled
}

#[repr(C)]
#[derive(Default)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

impl RulesetAttr {
    /// The struct size this kernel accepts. The kernel validates the declared size against
    /// the ABI it implements, so a fixed `size_of::<RulesetAttr>()` is rejected with
    /// `E2BIG` on a kernel that predates the later fields.
    fn size_for(abi: u32) -> usize {
        match abi {
            0..=3 => 8,  // handled_access_fs only
            4..=5 => 16, // + handled_access_net
            _ => 24,     // + scoped
        }
    }
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// A built ruleset, ready to be enforced. Held by the PARENT across `fork`, enforced by
/// the child through [`restrict_self`] — splitting it this way is what keeps the
/// post-fork path to two raw syscalls with no allocation.
#[derive(Debug)]
pub(crate) struct LandlockRuleset {
    fd: OwnedFd,
    pub(crate) rules_added: usize,
}

impl LandlockRuleset {
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Surrender the descriptor to the caller, which must keep it open until the child is
    /// spawned — `landlock_restrict_self` runs after `fork` and needs it live.
    pub(crate) fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

/// The kernel's Landlock ABI, or `None` when the kernel has no Landlock at all
/// (`ENOSYS` on <5.13, or a kernel built without it / with it absent from `lsm=`).
pub(crate) fn probe_abi() -> Option<u32> {
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    u32::try_from(rc).ok().filter(|abi| *abi >= MIN_FS_ABI)
}

/// One resolved grant: the path to open and the rights to attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandlockGrant {
    pub(crate) path: PathBuf,
    pub(crate) access: LandlockAccess,
}

/// Derive the full grant list for `policy`.
///
/// The policy's own rules come from [`compile_mount_plan`] — the identical derivation the
/// bubblewrap backend uses, so glob reduction, absent-speculative tolerance, and
/// deny-shadow dropping behave the same on both mechanisms. The system closure and device
/// nodes are added because Landlock has no root view: bubblewrap starts from an empty
/// mount namespace and BUILDS UP a minimal root, whereas a Landlock child still sees the
/// whole host filesystem and is restricted only by what it may open. Everything the
/// bubblewrap backend gets implicitly from `RootView::Minimal` must therefore be an
/// explicit grant here.
pub(crate) fn derive_grants(
    policy: &SandboxPolicy,
    tmp_dir: Option<&Path>,
    entry_program: Option<&Path>,
) -> Result<Vec<LandlockGrant>, String> {
    // THE CATALOG'S FULL-DISK TIER, and the only fs shape this backend answers with a single
    // rule. An fs axis that confines nothing (`entries: []`, `default_effect: Allow`) is what
    // `preset::relax_fs_to_full_disk` compiles a `fullDisk` grant to; Landlock cannot express
    // it by ABSENCE, because a child under a ruleset sees the whole host filesystem and is
    // restricted by what the ruleset omits — so an empty ruleset denies EVERYTHING, the exact
    // inverse. One rule on `/` carrying every handled right is the expression, and the rest of
    // the confinement (the seccomp socket ceiling, `setsid`, the descriptor sweep, the
    // capability drop) is untouched because none of it rides the fs ruleset. Returned before
    // the system closure and device grants are added: they are all nested under `/`, so
    // appending them would emit rules that grant strictly less than the one above them.
    if !crate::backend::linux_grants::fs_confines(&policy.fs) {
        return Ok(vec![LandlockGrant {
            path: PathBuf::from("/"),
            access: LandlockAccess::FullDisk,
        }]);
    }
    let plan = compile_mount_plan(policy)?;
    reject_narrowing_grants(&plan)?;

    let mut grants: Vec<LandlockGrant> = Vec::new();
    for path in system_read_paths() {
        grants.push(LandlockGrant {
            path: PathBuf::from(path),
            access: LandlockAccess::ReadExecute,
        });
    }
    for path in PROC_READ_PATHS {
        grants.push(LandlockGrant {
            path: PathBuf::from(path),
            access: LandlockAccess::ReadExecute,
        });
    }
    // The entry program in its own right. Bubblewrap bound it separately from the read floor
    // so an interpreter living outside that floor — nub's provisioned Node under its own
    // store, a CI runner's `/opt/hostedtoolcache` Node — stays launchable however narrow the
    // floor is; without the equivalent here `execve` fails with EACCES before the script runs.
    if let Some(program) = entry_program {
        grants.push(LandlockGrant {
            path: program.to_path_buf(),
            access: LandlockAccess::ReadExecute,
        });
    }
    // `TmpMode::Private` gives the jail a per-run scratch dir, which bubblewrap bound over
    // `/tmp`. There is no mount namespace to rebind here, so the child gets the host path
    // granted directly and `TMPDIR` pointed at it. Without this the whole toolchain loses its
    // temp dir — `gcc` writes `/tmp/ccXXXXXX.s`, `sh` writes here-docs — and every native
    // build fails for a reason no denial message would explain.
    if let Some(tmp) = tmp_dir {
        grants.push(LandlockGrant {
            path: tmp.to_path_buf(),
            access: LandlockAccess::ReadWrite,
        });
    }
    for path in DEVICE_PATHS {
        grants.push(LandlockGrant {
            path: PathBuf::from(path),
            access: LandlockAccess::Device,
        });
    }
    for grant in plan {
        grants.push(LandlockGrant {
            path: grant.path,
            access: match grant.access {
                MountAccess::ListOnly => LandlockAccess::ListDir,
                MountAccess::ReadOnly => LandlockAccess::ReadExecute,
                MountAccess::ReadWrite => LandlockAccess::ReadWrite,
            },
        });
    }
    Ok(grants)
}

/// Refuse a plan whose later grant NARROWS an earlier one.
///
/// Bubblewrap applies binds in order, so "writable parent, read-only child" is a real and
/// supported shape there. Landlock unions its rules, so the same pair yields a WRITABLE
/// child — silently wider than the policy asked for. The build jail never authors that
/// shape (its plan is a read-only dependency tree with the package dir writable INSIDE it,
/// which unions correctly), so this is a guard against a future policy change quietly
/// losing enforcement, not a limitation being worked around. Fail closed instead of
/// under-enforcing.
fn reject_narrowing_grants(plan: &[MountGrant]) -> Result<(), String> {
    for (index, grant) in plan.iter().enumerate() {
        // Every narrowing shape, not just `ReadOnly`: a `ListOnly` node nested in a
        // writable grant is the same non-restriction, and skipping it would let the
        // guard pass on a plan it exists to refuse.
        if grant.access == MountAccess::ReadWrite {
            continue;
        }
        if let Some(wider) = plan[..index].iter().find(|earlier| {
            earlier.access == MountAccess::ReadWrite && grant.path.starts_with(&earlier.path)
        }) {
            return Err(format!(
                "landlock cannot express the read-only cap {} inside the writable grant {}: \
                 rules union, so the cap would not restrict",
                grant.path.display(),
                wider.path.display()
            ));
        }
    }
    Ok(())
}

/// Build the ruleset in the PARENT: create it, attach every grant, and hand back the fd.
///
/// Absent paths are skipped rather than refused. The mount planner has already refused an
/// absent AUTHORED policy path; what reaches here additionally includes the system closure,
/// which is deliberately distro-spanning (`/libx32`, `/etc/pki`, `/etc/crypto-policies`
/// exist almost nowhere at once), so refusing on absence would abort every confined run.
pub(crate) fn build(
    policy: &SandboxPolicy,
    tmp_dir: Option<&Path>,
    entry_program: Option<&Path>,
) -> Result<LandlockRuleset, String> {
    let abi = probe_abi().ok_or_else(|| "landlock is not available on this kernel".to_string())?;
    let grants = derive_grants(policy, tmp_dir, entry_program)?;

    let attr = RulesetAttr {
        handled_access_fs: handled_access_fs(abi),
        ..RulesetAttr::default()
    };
    let raw = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const RulesetAttr,
            RulesetAttr::size_for(abi),
            0u32,
        )
    };
    let fd = RawFd::try_from(raw)
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| {
            format!(
                "landlock_create_ruleset: {}",
                std::io::Error::last_os_error()
            )
        })?;
    // SAFETY: the syscall returned a fresh, owned descriptor.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut rules_added = 0usize;
    for grant in &grants {
        if add_rule(ruleset.as_raw_fd(), grant, abi)? {
            rules_added += 1;
        }
    }
    Ok(LandlockRuleset {
        fd: ruleset,
        rules_added,
    })
}

/// Attach one grant. Returns whether a rule was actually added (`false` = path absent).
fn add_rule(ruleset_fd: RawFd, grant: &LandlockGrant, abi: u32) -> Result<bool, String> {
    // `O_PATH` WITHOUT `O_NOFOLLOW` deliberately: it resolves symlinks, so a grant naming
    // `/etc/resolv.conf` keys the rule on the `/run/systemd/resolve/...` inode the read
    // actually reaches. Landlock evaluates the RESOLVED path, so following here is what
    // makes a symlinked leaf grant work at all.
    let Some(fd) = open_path(&grant.path) else {
        return Ok(false);
    };
    let mut rights = grant.access.rights(abi) & handled_access_fs(abi);
    if !is_directory(fd.as_raw_fd()) {
        rights &= FILE_ONLY_RIGHTS;
    }
    let attr = PathBeneathAttr {
        allowed_access: rights,
        parent_fd: fd.as_raw_fd(),
    };
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    if rc != 0 {
        return Err(format!(
            "landlock_add_rule for {}: {}",
            grant.path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(true)
}

fn open_path(path: &Path) -> Option<OwnedFd> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `open` returned a fresh, owned descriptor.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn is_directory(fd: RawFd) -> bool {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let rc = unsafe {
        libc::fstatat(
            fd,
            c"".as_ptr(),
            &mut stat as *mut libc::stat,
            libc::AT_EMPTY_PATH,
        )
    };
    rc == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
}

/// Linux capability-set header version 3 (64-bit caps, two 32-bit data blocks).
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Drop every capability from the calling thread — the analogue of bubblewrap's
/// `--cap-drop ALL`, which the Landlock path loses along with the user namespace.
///
/// This matters precisely where this backend is most needed. `nub install` inside a
/// container commonly runs as root, and Docker's default set still carries `CAP_CHOWN`,
/// `CAP_FOWNER`, `CAP_DAC_OVERRIDE` and `CAP_MKNOD`. Landlock does not mediate `chmod`,
/// `chown`, `setxattr` or `utime` at any ABI — no ABI ever has, and upstream tracks the
/// missing hook as `landlock-lsm/linux#11` — so DAC is what stands between a dependency's
/// install script and host-wide metadata rewriting, and `CAP_DAC_OVERRIDE` removes DAC.
/// Unprivileged callers hold nothing to drop and this is a no-op for them.
///
/// Two of those four are now ALSO seccomp-denied for build-jail launches (`deny_metadata`
/// in `linux.rs`'s `build_seccomp`), so DAC is no longer the only lever against `chown`
/// and `setxattr`. `chmod` and `utime` still ride on it alone: denying either breaks real
/// packages, and seccomp cannot scope a denial to a path. This drop is what covers them
/// when the caller is root.
///
/// The BOUNDING set is dropped first, because doing so needs `CAP_SETPCAP` in the effective
/// set; zeroing the sets first would make the bounding drop fail. A bounding drop that fails
/// for lack of privilege is not an error — it means there was no capability to remove.
///
/// # Safety
/// Must be called between `fork` and `execve`. Performs only raw syscalls.
unsafe fn drop_all_capabilities() -> Result<(), std::io::Error> {
    // 64 covers every capability the kernel defines; past the last one `prctl` returns
    // EINVAL, which is the loop's natural terminator and not a failure.
    for cap in 0..64 {
        unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
    }
    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
    if unsafe { libc::syscall(libc::SYS_capset, &header as *const CapHeader, data.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Enforce the ruleset on the CALLING thread and its future children. Async-signal-safe:
/// two raw syscalls, no allocation, nothing that can take a lock the forking parent held.
///
/// The caller is responsible for `PR_SET_NO_NEW_PRIVS` — Landlock refuses to restrict a
/// process that could still gain privileges through a setuid `execve`. The Linux target
/// child already sets it (and verifies it took) before this point, so it is asserted here
/// rather than set again.
///
/// # Safety
/// Must be called between `fork` and `execve` in the child, with `no_new_privs` already
/// set. `ruleset_fd` must be a live Landlock ruleset descriptor.
pub(crate) unsafe fn restrict_self(ruleset_fd: RawFd) -> Result<(), libc::c_int> {
    if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        return Err(libc::EPERM);
    }
    let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL));
    }
    Ok(())
}

/// Why the Landlock mechanism cannot be used for a given policy/host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LandlockUnavailable {
    /// Kernel below 5.13, or built without Landlock / with it absent from `lsm=`.
    NoKernelSupport,
    /// The policy carries a deny rule. Landlock unions rules and has no deny primitive at
    /// any ABI, so a deny is inexpressible — it would silently not restrict.
    PolicyHasDenyRules,
    /// The grant shape itself is inexpressible (see [`reject_narrowing_grants`]), or the
    /// mount plan refused.
    PolicyNotExpressible(String),
    /// `NUB_SANDBOX_MECHANISM=bubblewrap` pinned the selector for a differential run.
    PinnedToBubblewrap,
    /// A `nub sandbox` scope rather than the build jail. Out of scope by design.
    NotABuildJail,
}

/// Whether Landlock can confine `policy` on this host, and at what ABI.
///
/// THE BUILD JAIL HAS NO OTHER MECHANISM. Bubblewrap is not a fallback here, not even where
/// it happens to work: it needs a user namespace, unprivileged availability of which is not
/// universal, and universal unprivileged operation is the requirement that defines this
/// product. Bubblewrap belongs to `nub sandbox`, which pays for it with escalation. So this
/// is an AVAILABILITY question, not a mechanism-selection one — there is nothing to select
/// between, and the caller fails the launch closed on `Err`.
///
/// BELOW THE KERNEL FLOOR (Landlock is 5.13, mid-2021) the answer is REFUSE, not
/// run-unconfined-with-a-warning. The jail's contract everywhere else is fail-closed, and a
/// dependency's install script is precisely the code whose whole reason for being confined is
/// that it is untrusted — running it unconfined because the kernel is old inverts the
/// product. A warning is not a substitute: it is printed to a log nobody reads, after the
/// script has already run. The affected population is a small and shrinking tail, and it gets
/// an actionable error naming the kernel requirement rather than silent exposure.
pub(crate) fn landlock_availability(policy: &SandboxPolicy) -> Result<u32, LandlockUnavailable> {
    // INTERNAL mechanism pin, for differential testing only. The two backends enforce the
    // same policy through different primitives, so "did behaviour change?" is only
    // answerable by running both on ONE host — which needs a way to hold the selector
    // still. Not a user knob and not documented as one. It is the ONLY way a build-jail
    // spawn can reach bubblewrap: the production path has no bubblewrap arm at all.
    let pinned_to_landlock = match std::env::var("NUB_SANDBOX_MECHANISM").as_deref() {
        Ok("bubblewrap") => return Err(LandlockUnavailable::PinnedToBubblewrap),
        // A HARD pin: a differential arm that silently fell back would compare the mechanism
        // against itself, so the scope gate below is bypassed and any real unavailability
        // surfaces as an error rather than a quiet substitution.
        Ok("landlock") => true,
        _ => false,
    };
    // SCOPE GATE, and it runs in BOTH directions. Landlock is the build jail's mechanism and
    // only its mechanism: a `nub sandbox` scope needs deny-inside-allow plus the mount/PID/net
    // namespaces, and enforcing one here would silently drop every namespace-backed axis it
    // depends on — the tests for those axes fail loudly under Landlock precisely because the
    // mechanism cannot carry them.
    if !policy.build_jail && !pinned_to_landlock {
        return Err(LandlockUnavailable::NotABuildJail);
    }
    let abi = probe_abi().ok_or(LandlockUnavailable::NoKernelSupport)?;
    // An INVARIANT check, not a routing decision. `enforce_pure_allowlist` strips every deny
    // from a build-jail policy, so a deny reaching here means that guarantee broke upstream —
    // and Landlock would union the rule away to nothing rather than enforce it. Refuse loudly
    // instead of enforcing something weaker than the policy says.
    if policy
        .fs
        .rules
        .entries
        .iter()
        .any(|rule| rule.effect == crate::policy::Effect::Deny)
    {
        return Err(LandlockUnavailable::PolicyHasDenyRules);
    }
    derive_grants(policy, None, None).map_err(LandlockUnavailable::PolicyNotExpressible)?;
    Ok(abi)
}

/// Install the child-side confinement hook on `command`: Landlock, then the syscall filter,
/// then the descriptor sweep, all between `fork` and `execve`.
///
/// This is the whole reason the Landlock path cannot reuse the bubblewrap monitor. That
/// monitor is itself launched THROUGH bubblewrap (`--unshare-user --unshare-pid …`), so it
/// needs the very user namespace this mechanism exists to avoid — which means the three
/// things it did for the target (no-new-privs, the seccomp install, and the descriptor
/// sweep) have to be re-established here.
///
/// THE DESCRIPTOR SWEEP IS LOAD-BEARING, not hygiene. An fd nub already holds open — a
/// registry socket, the proxy connection, a log file — is inherited by the jailed script and
/// is usable WITHOUT reopening it, so it passes straight through both layers: Landlock
/// governs `open`, not an already-open descriptor, and seccomp's `socket()` ceiling never
/// sees a syscall. A descriptor egressing this way was MEASURED during the prototype. It is
/// marked CLOEXEC rather than closed so the child's exec-error report — the pipe Rust's own
/// spawn machinery relies on to tell the parent that `execve` failed — still works; `execve`
/// then closes the whole marked range atomically.
///
/// # Safety
/// `ruleset_fd` must outlive the spawn. The caller retains the [`LandlockRuleset`] on the
/// returned `Prepared` for exactly that reason.
unsafe fn install_confinement_pre_exec(
    command: &mut Command,
    ruleset_fd: RawFd,
    seccomp: Option<std::sync::Arc<Vec<seccompiler::sock_filter>>>,
) {
    use std::os::unix::process::CommandExt;
    let hook = move || -> std::io::Result<()> {
        // `setsid`, NOT `setpgid` — it detaches the CONTROLLING TERMINAL as well as
        // starting a new process group. Bubblewrap got this from `--new-session`, whose
        // comment in linux.rs records a MEASURED result: with every other flag present but
        // that one removed, a confined child holding the launcher's tty can `ioctl(TIOCSTI)`
        // bytes into the parent shell's input queue, to be executed OUTSIDE the sandbox.
        // Seccomp cannot catch it (TIOCSTI is an ioctl request, not a syscall), so
        // relinquishing the terminal is the whole defence. The new session also gives the
        // parent a process GROUP to signal, which is its only handle on descendants without
        // a PID namespace. Safe here because a freshly forked child is never already a
        // process-group leader, which is the sole `EPERM` case.
        if unsafe { libc::setsid() } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // FIRST, before any restriction is installed: the sweep's fallback path opens
        // `/proc/self/fd`, which the ruleset below makes unreadable. Ordering it here keeps
        // that fallback usable on a kernel without `CLOSE_RANGE_CLOEXEC`.
        super::linux_monitor::mark_inherited_fds_cloexec()?;
        // Both Landlock and seccomp REFUSE an unprivileged caller that could still gain
        // privileges through a setuid execve, so this gates everything below it.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { drop_all_capabilities() }?;
        unsafe { restrict_self(ruleset_fd) }.map_err(std::io::Error::from_raw_os_error)?;
        if let Some(filter) = &seccomp {
            super::linux_monitor::install_target_seccomp(filter)
                .map_err(std::io::Error::from_raw_os_error)?;
        }
        Ok(())
    };
    // SAFETY: the hook runs between fork and execve and performs only raw syscalls — no
    // allocation, and nothing that can take a lock the forking parent held.
    unsafe { command.pre_exec(hook) };
}

/// Build the fully-confined child command for the Landlock mechanism.
///
/// Returns the command plus the ruleset, which the caller MUST retain until the child is
/// spawned — the descriptor is consumed by `landlock_restrict_self` after `fork`.
pub(crate) fn prepare_launch(
    policy: &SandboxPolicy,
    mut command: Command,
    seccomp: Option<Vec<seccompiler::sock_filter>>,
    tmp_dir: Option<&Path>,
    entry_program: Option<&Path>,
) -> Result<(Command, LandlockRuleset), String> {
    let ruleset = build(policy, tmp_dir, entry_program)?;
    let fd = ruleset.as_raw_fd();
    unsafe { install_confinement_pre_exec(&mut command, fd, seccomp.map(std::sync::Arc::new)) };
    Ok((command, ruleset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, FsRuleSet};

    fn derive_grants_for_test(p: &SandboxPolicy) -> Result<Vec<LandlockGrant>, String> {
        derive_grants(p, None, None)
    }

    fn rule(path: &str, effect: Effect, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(path.to_string()),
            effect,
            access,
            origin: FsOrigin::Authored,
        }
    }

    /// A subtree the way the compiler spells it — the node and its `/**` twin. A bare path
    /// with no twin is the directory NODE, a different grant ([`MountAccess::ListOnly`]),
    /// so a fixture meaning "this tree" has to say so.
    fn subtree(path: &str, effect: Effect, access: FsAccess) -> [FsRule; 2] {
        [
            rule(path, effect, access),
            rule(&format!("{path}/**"), effect, access),
        ]
    }

    fn policy(entries: Vec<FsRule>) -> SandboxPolicy {
        let mut policy = SandboxPolicy::default();
        policy.fs.rules = FsRuleSet {
            entries,
            default_effect: Effect::Deny,
        };
        policy
    }

    /// THE regression this backend exists to not re-break. A bubblewrap `--ro-bind` grants
    /// execute implicitly; Landlock does not, and a read grant that omits EXECUTE makes
    /// every compiler, shell, and interpreter under it fail `execve` with EACCES — which
    /// took out native addon builds wholesale during the prototype.
    #[test]
    fn a_read_grant_carries_execute() {
        for abi in 1..=7 {
            let rights = LandlockAccess::ReadExecute.rights(abi);
            assert!(
                rights & ACCESS_EXECUTE != 0,
                "ABI {abi}: a read grant must carry EXECUTE or every binary under it \
                 becomes unexecutable"
            );
            assert!(rights & ACCESS_READ_FILE != 0 && rights & ACCESS_READ_DIR != 0);
            assert!(
                rights & ACCESS_WRITE_FILE == 0,
                "ABI {abi}: a read grant must not carry write"
            );
        }
    }

    /// Rights the running kernel does not implement must not be declared: an unhandled
    /// right is entirely unrestricted, so declaring a right the ABI lacks is not merely
    /// inert, and asking for one it does not know is rejected outright.
    #[test]
    fn access_sets_stay_within_the_abis_handled_rights() {
        for abi in 1..=7 {
            let handled = handled_access_fs(abi);
            for access in [
                LandlockAccess::ListDir,
                LandlockAccess::ReadExecute,
                LandlockAccess::ReadWrite,
                LandlockAccess::Device,
            ] {
                assert_eq!(
                    access.rights(abi) & !handled,
                    0,
                    "ABI {abi}: {access:?} asks for a right this ABI does not handle"
                );
            }
        }
        assert_eq!(handled_access_fs(1) & ACCESS_REFER, 0);
        assert_eq!(handled_access_fs(2) & ACCESS_REFER, ACCESS_REFER);
        assert_eq!(handled_access_fs(2) & ACCESS_TRUNCATE, 0);
        assert_eq!(handled_access_fs(4) & ACCESS_IOCTL_DEV, 0);
        assert_eq!(handled_access_fs(5) & ACCESS_IOCTL_DEV, ACCESS_IOCTL_DEV);
    }

    /// The kernel validates the declared attr size against its own ABI, so this must grow
    /// with the ABI rather than always being `size_of::<RulesetAttr>()`.
    #[test]
    fn ruleset_attr_size_tracks_the_abi() {
        assert_eq!(RulesetAttr::size_for(1), 8);
        assert_eq!(RulesetAttr::size_for(4), 16);
        assert_eq!(RulesetAttr::size_for(6), 24);
        assert_eq!(RulesetAttr::size_for(6), std::mem::size_of::<RulesetAttr>());
    }

    /// Landlock unions rules, so a read-only cap nested in a writable grant does not
    /// restrict. Refuse rather than silently hand back write access.
    #[test]
    fn a_read_only_cap_inside_a_writable_grant_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let error = derive_grants_for_test(&policy(
            [
                subtree(
                    &parent.to_string_lossy(),
                    Effect::Allow,
                    FsAccess::ReadWrite,
                ),
                subtree(&child.to_string_lossy(), Effect::Allow, FsAccess::Read),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ))
        .expect_err("a narrowing cap must be refused, not silently widened");
        assert!(
            error.contains("rules union"),
            "the refusal must name the mechanism: {error}"
        );
    }

    /// The build jail's own shape — a read-only dependency tree with the package dir
    /// writable INSIDE it — unions correctly and must keep compiling. Control for the test
    /// above: without this, the guard could reject everything and still pass.
    #[test]
    fn a_writable_grant_inside_a_read_only_tree_still_compiles() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("node_modules");
        let package = tree.join("native");
        std::fs::create_dir_all(&package).unwrap();

        let grants = derive_grants_for_test(&policy(
            [
                subtree(&tree.to_string_lossy(), Effect::Allow, FsAccess::Read),
                subtree(
                    &package.to_string_lossy(),
                    Effect::Allow,
                    FsAccess::ReadWrite,
                ),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ))
        .expect("the build jail's own nesting must compile");
        assert_eq!(
            grants
                .iter()
                .find(|g| g.path == tree)
                .map(|g| g.access)
                .expect("the dependency tree is granted"),
            LandlockAccess::ReadExecute
        );
        assert_eq!(
            grants
                .iter()
                .find(|g| g.path == package)
                .map(|g| g.access)
                .expect("the package dir is granted"),
            LandlockAccess::ReadWrite
        );
    }

    /// The scope boundary between the two products. A `nub sandbox` scope leans on the
    /// mount/PID/net namespaces Landlock does not have, so routing one here would silently
    /// drop those axes; only the build jail is eligible.
    #[test]
    fn only_a_build_jail_policy_is_landlock_eligible() {
        let mut scope = policy(Vec::new());
        assert_eq!(
            landlock_availability(&scope),
            Err(LandlockUnavailable::NotABuildJail),
            "a nub sandbox scope must never be routed to landlock"
        );
        scope.build_jail = true;
        assert_ne!(
            landlock_availability(&scope),
            Err(LandlockUnavailable::NotABuildJail),
            "control: the same policy marked as the build jail clears the scope gate"
        );
    }

    /// Landlock unions rules and cannot subtract, so a policy carrying any deny must fall
    /// back rather than be enforced with the deny silently dropped.
    #[test]
    fn a_deny_rule_disqualifies_landlock() {
        let mut denied = policy(vec![rule("/tmp/secret", Effect::Deny, FsAccess::DENY)]);
        denied.build_jail = true;
        assert_eq!(
            landlock_availability(&denied),
            Err(LandlockUnavailable::PolicyHasDenyRules)
        );
    }

    /// Landlock has no root view, so everything bubblewrap gets from `RootView::Minimal`
    /// has to be an explicit rule. A policy that granted only its own paths would leave the
    /// child unable to exec the loader.
    #[test]
    fn the_system_closure_and_devices_are_granted_explicitly() {
        let grants = derive_grants_for_test(&policy(Vec::new())).unwrap();
        for required in ["/usr", "/etc/resolv.conf", "/dev/null"] {
            assert!(
                grants.iter().any(|g| g.path == Path::new(required)),
                "{required} must be granted explicitly — Landlock builds no minimal root"
            );
        }
        assert_eq!(
            grants
                .iter()
                .find(|g| g.path == Path::new("/dev/null"))
                .map(|g| g.access),
            Some(LandlockAccess::Device),
            "a device node is read/write but never executable"
        );
    }
}
