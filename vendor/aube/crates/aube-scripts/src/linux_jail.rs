use crate::ScriptJail;
use landlock::{
    ABI, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetStatus,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;
use std::path::Path;

fn add_rule(
    ruleset: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, String> {
    let fd = PathFd::new(path)
        .map_err(|e| format!("failed to open jail allow path {}: {e}", path.display()))?;
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| format!("failed to add jail allow path {}: {e}", path.display()))
}

fn add_rule_with_canonical(
    mut ruleset: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, String> {
    ruleset = add_rule(ruleset, path, access)?;
    if let Ok(canonical) = path.canonicalize()
        && canonical != path
    {
        ruleset = add_rule(ruleset, &canonical, access)?;
    }
    Ok(ruleset)
}

pub(crate) fn apply_landlock(jail: &ScriptJail, home: &Path) -> Result<(), String> {
    // Must run before restrict_self() so a setuid exec inside the jail
    // cannot pick up privileges that would shadow the Landlock domain.
    // Both restrict_self() and seccomp's SET_MODE_FILTER refuse to load
    // without it for an unprivileged caller.
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(format!(
            "failed to set PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Two compat tiers, and the split is the whole design.
    //
    // ABI v2 (kernel >= 5.19) is the HARD FLOOR: it carries every write
    // restriction this policy's threat model depends on, and a kernel that
    // cannot enforce all of it must fail the script rather than run it
    // half-confined.
    //
    // v3 (kernel >= 6.2) adds exactly one right, LANDLOCK_ACCESS_FS_TRUNCATE,
    // and without it a confined script can truncate any file it can name --
    // Landlock's other write hooks never see truncate(2)/ftruncate(2)/O_TRUNC.
    // Requiring v3 outright would fix that at the cost of refusing to confine
    // the LTS kernels still shipping 5.15-6.1 (Ubuntu 22.04, Debian 12, RHEL 9),
    // which is not a trade a package manager gets to make. So it is requested
    // BEST-EFFORT on top of the floor: enforced wherever the kernel has it,
    // silently dropped where it does not, which is Landlock's own stated
    // design intent for feature adoption.
    //
    // The ordering matters: set_compatibility applies to subsequent calls, so
    // the floor is handled under HardRequirement and everything after -- the
    // truncate bit and every add_rule below -- is best-effort. That is what
    // makes the relaxed restrict_self() check at the bottom safe to write.
    let floor_abi = ABI::V2;
    let read_access = AccessFs::from_read(floor_abi);
    let floor_access = read_access | AccessFs::from_write(floor_abi);
    let full_access = floor_access | AccessFs::Truncate;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(floor_access)
        .map_err(|e| format!("failed to create jail ruleset: {e}"))?
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::Truncate)
        .map_err(|e| format!("failed to create jail ruleset: {e}"))?
        .create()
        .map_err(|e| format!("failed to create jail ruleset: {e}"))?;

    ruleset = add_rule(ruleset, Path::new("/"), read_access)?;
    // `home` already has `full_access` and `apply_jail_env` points
    // TMPDIR/TMP/TEMP at it, so build scripts get a writable scratch
    // dir without granting kernel-level write to the world-writable
    // system `/tmp`. Granting `/tmp` would let a script read another
    // tenant's tmp files or seed symlink races on shared CI hosts.
    for path in [Path::new("/dev"), jail.package_dir.as_path(), home] {
        ruleset = add_rule_with_canonical(ruleset, path, full_access)?;
    }
    for path in &jail.write_paths {
        ruleset = add_rule_with_canonical(ruleset, path, full_access)?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| format!("failed to apply jail filesystem rules: {e}"))?;
    // PartiallyEnforced can only mean a BEST-EFFORT right was dropped, never a
    // floor one: the floor is a HardRequirement, so a kernel missing any of it
    // returns Err above and never reaches here. Insisting on FullyEnforced would
    // therefore turn the best-effort truncate request into a hard v3 floor --
    // failing shut on exactly the 5.19-6.1 LTS kernels the split exists to keep.
    // NotEnforced still refuses to run the script.
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(format!(
            "jail filesystem rules were not enforced: {:?}",
            status.landlock
        ));
    }
    Ok(())
}

/// Installs the jail's seccomp filter: the metadata-mutation denials always,
/// and the socket-family denials only when the grant did not ask for network.
///
/// Both families share one filter because `SeccompFilter`'s match action is
/// global, and EPERM is the right answer for both.
pub(crate) fn apply_seccomp_filter(network_allowed: bool) -> Result<(), String> {
    let target_arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for jail seccomp filter: {e}"))?;
    // seccompiler's mismatch_action is the default for every syscall
    // the BPF program sees, not just the ones in `rules`. Setting it
    // to Errno would make every non-socket syscall (open, write, mmap,
    // ...) also return EPERM and kill the jailed shell at startup.
    // Pure default-deny on the socket family axis would require
    // libseccomp's per-syscall default action, which seccompiler does
    // not expose. Until that lands, enumerate the dangerous families
    // explicitly and keep AF_UNIX flowing for node + node-gyp IPC
    // (socketpair stdio, worker_threads).
    //
    // Known residual gap: AF_UNIX `connect()` to filesystem socket
    // paths (e.g. /var/run/docker.sock) is NOT covered by Landlock
    // ABI v2. The v2 policy only filters VFS open/write hooks, the
    // unix socket connect path bypasses them by passing sun_path
    // inline in sockaddr_un. LANDLOCK_ACCESS_FS_CONNECT_UNIX lands
    // in v5 (kernel 6.10+); the policy here pins v2 to keep LTS
    // kernels (Ubuntu 22.04, Debian 12, RHEL 9) covered. On a host
    // with /var/run/docker.sock and a kernel below 6.10, an approved
    // postinstall could still reach the docker daemon. Mitigations:
    // run installs in a namespace where the socket is bind-mounted
    // away, or set jail.network=true and rely on host firewalling.
    let mk_family_rule = |family: i32| -> Result<SeccompRule, String> {
        SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, family as u64)
                .map_err(|e| format!("failed to build jail seccomp filter: {e}"))?,
        ])
        .map_err(|e| format!("failed to build jail seccomp filter: {e}"))
    };
    let denied_families = [
        libc::AF_INET,
        libc::AF_INET6,
        libc::AF_NETLINK,
        libc::AF_PACKET,
        libc::AF_VSOCK,
        libc::AF_XDP,
        libc::AF_ALG,
        libc::AF_BLUETOOTH,
        libc::AF_RDS,
        libc::AF_CAN,
        libc::AF_TIPC,
        libc::AF_IB,
        libc::AF_NFC,
    ];
    let mut family_rules = Vec::with_capacity(denied_families.len());
    for fam in denied_families {
        family_rules.push(mk_family_rule(fam)?);
    }

    let mut rules = BTreeMap::new();
    if !network_allowed {
        #[allow(clippy::useless_conversion)]
        for syscall in [libc::SYS_socket, libc::SYS_socketpair].map(i64::from) {
            rules.insert(syscall, family_rules.clone());
        }
    }

    // Metadata mutation. Landlock has no metadata hook at ANY ABI, so
    // ownership and xattr changes are otherwise entirely unmediated inside
    // the jail; seccomp is the only lever available without a mount namespace.
    //
    // The list is exactly what a denial matrix over five real native installs
    // (better-sqlite3, sqlite3, esbuild, simple-git-hooks, bufferutil) on
    // kernel 6.8 showed to be free, at BOTH uid 1000 and root. Two families
    // that measured zero calls are NOT here, because denying them broke
    // things anyway -- absence of calls in a sample is not absence of use:
    //   - `chmod`/`fchmodat` breaks 4 of the 5 with EPERM; node-gyp chmods the
    //     built addon from inside a make recipe, so denying it kills every
    //     from-source native build.
    //   - `utimensat` breaks sqlite3 and bufferutil, path or fd form alike.
    // Adding to this list therefore requires re-running that matrix, not just
    // an strace showing no calls.
    //
    // Be honest about the value: the two safe families are also the two with
    // the least attacker leverage (chown to another uid already fails under
    // DAC; `user.*` xattrs are inert), while `chmod` -- the one with teeth --
    // is the one packages genuinely need. This narrows the metadata surface;
    // it does not close it. See wiki/design/build-jail-linux.md.
    //
    // Errno, never KILL: unprivileged tar implementations already treat EPERM
    // from chown as routine, and SIGSYS would break what an errno does not.
    #[allow(unused_mut)] // arm64 takes no conditional extend below
    let mut metadata_denied = vec![
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fchownat,
    ];
    // x86_64 keeps the legacy path-based chown/lchown; the arm64 generic
    // syscall ABI dropped them, leaving fchownat as the only entry point.
    #[cfg(target_arch = "x86_64")]
    metadata_denied.extend([libc::SYS_chown, libc::SYS_lchown]);
    #[allow(clippy::useless_conversion)]
    for syscall in metadata_denied.into_iter().map(i64::from) {
        // No conditions: match every call to these syscalls.
        rules.insert(syscall, Vec::new());
    }

    // SeccompFilter::new arg order: rules, mismatch_action, match_action,
    // arch. mismatch_action is the global fallback (must be Allow so
    // unrelated syscalls keep flowing). match_action fires for the
    // listed denied families and returns EPERM, matching the errno
    // the original filter used and the test fixtures recognise.
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch,
    )
    .map_err(|e| format!("failed to build jail seccomp filter: {e}"))?
    .try_into()
    .map_err(|e| format!("failed to compile jail seccomp filter: {e}"))?;
    seccompiler::apply_filter(&filter)
        .map_err(|e| format!("failed to apply jail seccomp filter: {e}"))?;
    Ok(())
}
