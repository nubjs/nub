//! Linux zero-privilege enforcement: the Landlock build-jail backend and the shared
//! seccomp filter it installs.
//!
//! [`preflight`] decides the mechanism (Landlock or nothing — the bubblewrap backend was
//! removed with the curated zero-privilege import, epic 1.1), and [`apply`] launches the
//! Landlock arm through [`apply_landlock`]. Every non-Landlock policy is the epic 1.1(d)
//! seam, currently stubbed. [`build_seccomp`] compiles the socket/keyring/metadata ceiling
//! shared by that path.
#![cfg(target_os = "linux")]

use crate::backend::linux_grants::fs_confines;
use crate::backend::{CommandSpec, Degradation, Prepared};
use crate::policy::{Effect, SandboxPolicy, TmpMode};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch, sock_filter,
};
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    // AND THE SYMLINK TARGETS, because on most modern distros `/etc/resolv.conf` is a LINK and
    // the grant above then covers only the link. systemd-resolved points it at
    // `/run/systemd/resolve/stub-resolv.conf` (or `resolv.conf`), and `/var/run` is itself a link
    // to `/run`. Naming a path that does not exist is free — a Speculative mount source that is
    // missing is skipped — so spelling all of them costs nothing and closes the distro spread.
    //
    // WHY IT MATTERS: on Node <=8 `dns.js` builds a c-ares `ChannelWrap` at MODULE LOAD, reached
    // through `net.js` during `process.stdout` setup whenever stdio is a PIPE (everything node-gyp
    // spawns). A denied read surfaces as `Error: EFILE` and kills the lifecycle script, so the
    // package looks broken when only name resolution was ungranted. MEASURED on macOS, where the
    // same link hop caused it; these Linux spellings are the same shape and are UNVERIFIED here
    // for want of a Linux host in that session.
    "/run/systemd/resolve/stub-resolv.conf",
    "/run/systemd/resolve/resolv.conf",
    "/var/run/resolv.conf",
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

pub(crate) struct LinuxPreflight {
    /// Set when confinement is required but the Landlock mechanism was NOT selected — the
    /// seam the removed bubblewrap backend used to fill. epic 1.1(d) drives the seccomp
    /// user-notify supervisor into the case [`apply`] currently stubs. Distinct from "no
    /// confinement at all", which leaves both this and `landlock` unset.
    confine_without_landlock: bool,
    /// Set when the Landlock mechanism was selected. Landlock needs no bubblewrap candidate,
    /// no runtime image, and no namespace, so it is the only enforcement path this
    /// zero-privilege skeleton wires today.
    landlock: Option<LandlockPreflight>,
}

impl LinuxPreflight {
    /// Whether this launch will take the Landlock arm. Read by [`super::apply`] BEFORE it
    /// starts the egress proxy, because this mechanism can never route a child through one.
    pub(crate) fn uses_landlock(&self) -> bool {
        self.landlock.is_some()
    }
}

struct LandlockPreflight {
    abi: u32,
}
pub(crate) fn preflight(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
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
            confine_without_landlock: false,
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
                confine_without_landlock: false,
                landlock: Some(LandlockPreflight { abi }),
            });
        }
        // Fail closed on unavailable confinement. The historical differential pin now
        // selects the unprivileged supervisor, never the removed bubblewrap backend.
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
    // The bubblewrap backend that confined every non-Landlock policy was removed with the
    // curated zero-privilege import (epic 1.1). The two arms that fall through here — a policy
    // pinned to bubblewrap, and one that is not a build jail — have no Landlock mechanism to
    // use. Record that confinement is required WITHOUT Landlock and defer the decision to
    // `apply`, where epic 1.1(d) drives the seccomp user-notify supervisor.
    Ok(LinuxPreflight {
        confine_without_landlock: true,
        landlock: None,
    })
}

pub fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    tmp_dir: Option<&Path>,
    preflight: LinuxPreflight,
    // The loopback egress proxy's port + bearer, when one is running (a per-host net policy). The
    // Landlock build-jail arm ignores them — it has no supervisor to redirect and confines egress
    // with the coarse seccomp family ceiling — so they flow only into the supervised plan. (5.1)
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
) -> Result<Prepared, Degradation> {
    if let Some(landlock) = preflight.landlock {
        return apply_landlock(policy, spec, landlock, tmp_dir);
    }
    if preflight.confine_without_landlock {
        // The supervised (seccomp USER_NOTIF) launch — epic 1.1d/1.4. This is the seam the removed
        // bubblewrap backend filled: a policy that needs confinement but is not a build-jail
        // Landlock policy. NET is transparent per-host egress through the in-process supervisor;
        // FS (allow-only) rides a Landlock ruleset the child `restrict_self`s; write-intent ops
        // ride the USER_NOTIF broker. Private tmp is the per-run scratch dir `make_private_tmp`
        // created (threaded in as `tmp_dir`), granted rw by the ruleset + broker with `TMPDIR`
        // pointed at it; Deny tmp grants nothing, so the shared `/tmp` is simply never in the
        // allow-set. (Env is enforced by construction — `base_command`/`envp` — always.)
        let plan = build_supervised_plan(policy, &spec, tmp_dir, proxy_port, proxy_token)?;
        return Ok(Prepared {
            command: base_command(&spec, policy),
            degradation: Degradation::full(),
            proxy: None,
            _inherited_files: Vec::new(),
            signal_process_group: false,
            _private_tmp: None,
            redact_stdout: false,
            redact_stderr: false,
            supervised: Some(plan),
        });
    }
    // Nothing to confine: the policy enforces no axis, so hand back the plain child. Only the
    // Landlock path populates `_inherited_files`/`signal_process_group`; the removed bubblewrap
    // and retained-monitor fields are gone from `Prepared`.
    Ok(Prepared {
        command: base_command(&spec, policy),
        degradation: Degradation::full(),
        proxy: None,
        _inherited_files: Vec::new(),
        signal_process_group: false,
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
        supervised: None,
    })
}

/// Build the [`super::SupervisedPlan`] a `confine_without_landlock` policy forks with. The net
/// axis becomes an [`EgressPolicy`](super::linux_supervisor::EgressPolicy); the FS axis (allow-only)
/// becomes a Landlock ruleset the child `restrict_self`s; the environment is the same `constructed`
/// map `base_command` uses; argv0 is resolved to an absolute path for the bespoke `execve`. No
/// seccomp deny-ceiling yet — the connect-notifier already denies io_uring; keyctl/xattr/metadata
/// hardening for this path is a later 1.4 step.
fn build_supervised_plan(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
    tmp_dir: Option<&Path>,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
) -> Result<super::SupervisedPlan, Degradation> {
    let to_cstring = |bytes: &[u8], label: &str| -> Result<CString, Degradation> {
        CString::new(bytes).map_err(|_| Degradation {
            lost: vec!["process-input".to_string()],
            reason: Some(format!("sandbox {label} contains a NUL byte")),
        })
    };
    // Resolve argv0 to an absolute path the same way the Landlock path does, so the bespoke
    // `execve` (which performs no PATH search) can find it; fall back to the name verbatim so
    // `execve` fails closed in the child when it cannot be resolved.
    let child_cwd = spec
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let program_abs = resolve_program(&spec.program, &child_cwd, target_path(policy).as_deref())
        .unwrap_or_else(|| PathBuf::from(&spec.program));
    let mut argv = vec![to_cstring(
        program_abs.as_os_str().as_bytes(),
        "entry program",
    )?];
    for arg in spec.args.tokens() {
        argv.push(to_cstring(arg.as_bytes(), "argument")?);
    }
    let mut envp = Vec::with_capacity(policy.env.constructed.len() + 3);
    // A private tmp overrides the temp-dir env so tools write the per-run scratch dir, never the
    // shared `/tmp` (which the allow-set does not grant). Drop any constructed temp key first so
    // the child sees no duplicate — `execve` env with a repeated key is undefined.
    let is_tmp_key = |k: &str| tmp_dir.is_some() && matches!(k, "TMPDIR" | "TMP" | "TEMP");
    for (key, value) in &policy.env.constructed {
        if is_tmp_key(key) {
            continue;
        }
        envp.push(to_cstring(
            format!("{key}={value}").as_bytes(),
            "environment entry",
        )?);
    }
    if let Some(tmp) = tmp_dir {
        let tmp = tmp.to_string_lossy();
        for key in ["TMPDIR", "TMP", "TEMP"] {
            envp.push(to_cstring(
                format!("{key}={tmp}").as_bytes(),
                "temp-dir env",
            )?);
        }
    }
    let cwd = match &spec.cwd {
        Some(dir) => Some(to_cstring(dir.as_os_str().as_bytes(), "working directory")?),
        None => None,
    };
    let net = &policy.net;
    // No `enforce` ⇒ net is unconfined (allow everything). `default_effect == Allow` ⇒ the policy
    // admits every host. Otherwise only the explicit Allow-Host rules pass; the supervisor dials
    // and splices those and refuses the rest at connect.
    let allow_all = !net.enforce || net.default_effect == Effect::Allow;
    let allow = net
        .rules
        .iter()
        .filter(|rule| rule.effect == Effect::Allow)
        .filter_map(|rule| match &rule.target {
            crate::policy::NetTarget::Host(host) => Some(host.clone()),
            _ => None,
        })
        .collect();
    // Allow-only FS boundary: build the same Landlock ruleset the build-jail path uses, granting
    // the authored allow-set plus the system read floor plus the entry program. `None` when the
    // policy does not confine the filesystem (a pure net/env policy) — the child then skips
    // `restrict_self`. The Landlock UNION cannot subtract, so a Deny rule INSIDE a granted
    // subtree (`.git/hooks`, `.git/config`, the policy file) is carried by the write broker below.
    let ruleset = if fs_confines(&policy.fs) {
        Some(
            super::linux_landlock::build(policy, tmp_dir, Some(&program_abs)).map_err(
                |reason| Degradation {
                    lost: vec!["fs".to_string()],
                    reason: Some(reason),
                },
            )?,
        )
    } else {
        None
    };
    // When fs is confined the write broker becomes THE write-intent authority (it performs opens
    // outside Landlock), so it carries the write-side of exactly what Landlock grants PLUS the
    // deny-inside-allow carve-outs — `write_broker_ruleset` derives both from the same grants the
    // ruleset above is built from. `None` for a pure net/env policy, so no write-intent syscall
    // is trapped. Armed in lock-step with the ruleset (both gate on `fs_confines`).
    let write_policy = if ruleset.is_some() {
        Some(
            super::linux_landlock::write_broker_ruleset(policy, tmp_dir, Some(&program_abs))
                .map_err(|reason| Degradation {
                    lost: vec!["fs".to_string()],
                    reason: Some(reason),
                })?,
        )
    } else {
        None
    };
    Ok(super::SupervisedPlan {
        egress: super::linux_supervisor::EgressPolicy {
            allow_all,
            allow,
            write_policy,
            proxy_port,
            proxy_token: proxy_token.map(str::to_string),
        },
        argv,
        envp,
        cwd,
        ruleset,
        seccomp_ceiling: None,
        setsid: true,
    })
}

pub(super) fn protects_ambient_credentials(policy: &SandboxPolicy) -> bool {
    policy.env.resolved && policy.env.enforce && !policy.env.withheld.is_empty()
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

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
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
        // Holds the ruleset descriptor open until the child is spawned; `pre_exec` consumes
        // it after fork, so dropping it any earlier would leave the hook restricting nothing.
        _inherited_files: vec![std::fs::File::from(ruleset.into_fd())],
        // The Landlock hook makes the child a session leader, so its descendants are
        // reachable as a process group — this path's only handle on them.
        signal_process_group: true,
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
        supervised: None,
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
    use std::os::unix::ffi::OsStringExt;
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
}
