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
    // Also needed on the network: true path, where the seccomp filter
    // (which used to set this) is skipped.
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(format!(
            "failed to set PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        ));
    }
    // ABI v2 (kernel >= 5.19) covers every write-restriction this policy
    // needs and unblocks the LTS kernels that ship 5.15-6.1 (Ubuntu 22.04,
    // Debian 12, RHEL 9). v3 only adds LANDLOCK_ACCESS_FS_TRUNCATE.
    let abi = ABI::V2;
    let read_access = AccessFs::from_read(abi);
    let full_access = read_access | AccessFs::from_write(abi);
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(full_access)
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
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "jail filesystem rules were not fully enforced: {:?}",
            status.landlock
        ));
    }
    Ok(())
}

pub(crate) fn apply_seccomp_net_filter() -> Result<(), String> {
    let target_arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for jail network filter: {e}"))?;
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
                .map_err(|e| format!("failed to build jail network filter: {e}"))?,
        ])
        .map_err(|e| format!("failed to build jail network filter: {e}"))
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
    #[allow(clippy::useless_conversion)]
    for syscall in [libc::SYS_socket, libc::SYS_socketpair].map(i64::from) {
        rules.insert(syscall, family_rules.clone());
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
    .map_err(|e| format!("failed to build jail network filter: {e}"))?
    .try_into()
    .map_err(|e| format!("failed to compile jail network filter: {e}"))?;
    seccompiler::apply_filter(&filter)
        .map_err(|e| format!("failed to apply jail network filter: {e}"))?;
    Ok(())
}
