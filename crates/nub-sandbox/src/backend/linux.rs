//! Linux backend built around Bubblewrap's private filesystem and PID views.
//!
//! Bubblewrap constructs the view; it does not copy project files. Read-only and
//! writable binds keep their original absolute paths and write directly to the host.
//! Exact deny paths are layered last so a writable project cannot re-expose them.
#![cfg(target_os = "linux")]

use crate::backend::linux_grants::{self, DerivedGrants, fs_confines};
use crate::backend::{CommandSpec, Degradation, Prepared};
use crate::matcher::path::{PathMatcher, canonicalize_including_nonexistent};
use crate::policy::{Effect, FsAccess, SandboxPolicy, TmpMode};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const ESSENTIAL_READ_DIRS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32", "/etc", "/opt",
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
}

struct BubblewrapLaunch {
    program: PathBuf,
    visible_path: PathBuf,
    executable: Option<File>,
}

/// Apply a resolved policy using Bubblewrap. The exact same operation can be nested:
/// an outer mount/PID view remains in force and the child adds a stricter view inside it.
pub fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&Path>,
    tmp_dir: Option<&Path>,
) -> Result<Prepared, Degradation> {
    let confine_fs = fs_confines(&policy.fs);
    let sandboxing =
        confine_fs || policy.net.enforce || policy.env.enforce || policy.fs.tmp != TmpMode::Shared;

    if !sandboxing {
        return Ok(Prepared {
            command: base_command(&spec, policy),
            degradation: Degradation::full(),
            proxy: None,
            _inherited_files: Vec::new(),
            _private_tmp: None,
        });
    }

    if unsafe { libc::geteuid() } == 0 {
        return Err(Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(
                "sandboxed Linux execution as UID 0 cannot preserve both capability removal and nested sandboxing"
                    .to_string(),
            ),
        });
    }

    let bwrap = find_bwrap().map_err(|reason| Degradation {
        lost: vec!["fs".to_string()],
        reason: Some(reason),
    })?;
    let root_view = root_view(policy);
    let cwd = spec
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let (write_grants, write_partial) = linux_grants::derive_write_mount_grants(policy);
    if write_partial {
        return Err(Degradation {
            lost: vec!["fs-write-partial".to_string()],
            reason: Some("write policy exceeded the concrete mount-plan budget".to_string()),
        });
    }
    for grant in &write_grants {
        pre_create(&grant.path);
    }

    let mut deny_search_roots = spec.deny_search_roots.clone();
    if deny_search_roots.is_empty() {
        deny_search_roots.push(cwd.clone());
    }
    let mut masks = collect_masks(policy, &deny_search_roots).map_err(|reason| Degradation {
        lost: vec!["fs-read-deny".to_string()],
        reason: Some(reason),
    })?;
    masks.extend(alternate_procfs_masks().map_err(|reason| Degradation {
        lost: vec!["proc".to_string()],
        reason: Some(reason),
    })?);
    masks.sort_by(|a, b| a.path.cmp(&b.path));
    masks.dedup_by(|a, b| a.path == b.path);
    let masks = masks
        .into_iter()
        .filter(|mask| !mask_already_enforced(mask))
        .collect::<Vec<_>>();

    let mut command = Command::new(&bwrap.program);
    command.args(["--die-with-parent", "--new-session", "--unshare-user"]);
    command.args(["--cap-drop", "ALL"]);
    // Deliberately do not disable further user namespaces: a sandboxed agent must be
    // able to invoke Nub again and add a stricter child sandbox.
    command.args(["--unshare-pid", "--unshare-ipc"]);

    match root_view {
        RootView::ReadWrite => {
            command.args(["--bind", "/", "/"]);
        }
        RootView::ReadOnly => {
            command.args(["--ro-bind", "/", "/"]);
        }
        RootView::Minimal => {
            append_minimal_read_mounts(
                &mut command,
                policy,
                &spec,
                ca_bundle,
                &bwrap.visible_path,
            )?;
        }
    };

    // Replace host devices and host process information immediately after the root
    // view. Policy masks are layered later, so an explicit deny below `/dev` or the
    // fresh `/proc` cannot be hidden by these ancestor mounts.
    command.args(["--dev", "/dev", "--proc", "/proc"]);

    match policy.fs.tmp {
        TmpMode::Shared => {}
        TmpMode::Private => {
            let Some(dir) = tmp_dir else {
                return Err(Degradation {
                    lost: vec!["tmp-private".to_string()],
                    reason: Some("private temporary directory could not be created".to_string()),
                });
            };
            command.arg("--bind").arg(dir).arg("/tmp");
        }
        TmpMode::Deny => {
            // Traverse-only lets a later explicit project bind under `/tmp` remain
            // reachable, while the empty read-only tmp itself cannot be listed or
            // written.
            command.args(["--perms", "111", "--tmpfs", "/tmp"]);
        }
    }

    for grant in &write_grants {
        command.arg("--bind").arg(&grant.path).arg(&grant.path);
    }

    let mut mask_sources = Vec::new();
    for mask in masks.iter().filter(|m| !m.path.is_dir()) {
        let source = open_inheritable_dev_null().map_err(|e| Degradation {
            lost: vec!["fs-read-deny".to_string()],
            reason: Some(format!("opening empty mask source: {e}")),
        })?;
        let fd = source.as_raw_fd().to_string();
        command
            .arg("--perms")
            .arg(match mask.kind {
                MaskKind::EmptyReadable => "444",
                MaskKind::Unreadable => "000",
            })
            .arg("--ro-bind-data")
            .arg(&fd)
            .arg(&mask.path);
        mask_sources.push(source);
    }
    for mask in masks.iter().filter(|m| m.path.is_dir()) {
        command
            .arg("--perms")
            .arg(match mask.kind {
                MaskKind::EmptyReadable => "555",
                MaskKind::Unreadable => "000",
            })
            .arg("--tmpfs")
            .arg(&mask.path)
            .arg("--remount-ro")
            .arg(&mask.path);
    }
    if policy.fs.tmp == TmpMode::Deny {
        // Delay this until child mounts under `/tmp` have been installed; otherwise
        // Bubblewrap cannot create their destination mountpoints.
        command.args(["--remount-ro", "/tmp"]);
    }

    let mut degradation = Degradation::full();
    if policy.net.enforce {
        // A route-less network namespace is the fail-safe floor. The follow-up bridge
        // will reconnect only the already-running, host-side filtered proxy.
        command.arg("--unshare-net");
        if proxy_port.is_some() {
            degradation.lost.push("net-per-host".to_string());
            degradation.reason = Some(
                "Bubblewrap proxy bridge not yet wired; network was denied completely".to_string(),
            );
        }
    }

    let seccomp_source = if policy.net.enforce {
        let source =
            write_seccomp_program(build_network_seccomp().map_err(|reason| Degradation {
                lost: vec!["net".to_string()],
                reason: Some(reason),
            })?)
            .map_err(|e| Degradation {
                lost: vec!["net".to_string()],
                reason: Some(format!("writing network filter: {e}")),
            })?;
        command.arg("--seccomp").arg(source.as_raw_fd().to_string());
        Some(source)
    } else {
        None
    };

    command.arg("--chdir").arg(&cwd).arg("--");
    command.arg(&spec.program).args(&spec.args);

    if policy.env.enforce {
        command.env_clear();
        for (key, value) in &policy.env.constructed {
            command.env(key, value);
        }
    }
    if let Some(port) = proxy_port {
        super::set_proxy_env(&mut command, port, proxy_token);
    }
    if let Some(bundle) = ca_bundle {
        super::set_ca_env(&mut command, bundle);
    }
    if policy.fs.tmp == TmpMode::Private {
        super::set_tmp_env(&mut command, Path::new("/tmp"));
    }
    let inherited_fds = mask_sources
        .iter()
        .chain(seccomp_source.iter())
        .chain(bwrap.executable.iter())
        .map(AsRawFd::as_raw_fd)
        .collect::<Vec<_>>();
    seal_inherited_fds(&mut command, inherited_fds);

    let mut inherited_files = Vec::with_capacity(2);
    inherited_files.extend(mask_sources);
    inherited_files.extend(seccomp_source);
    inherited_files.extend(bwrap.executable);

    Ok(Prepared {
        command,
        degradation,
        proxy: None,
        _inherited_files: inherited_files,
        _private_tmp: None,
    })
}

fn root_view(policy: &SandboxPolicy) -> RootView {
    if !fs_confines(&policy.fs) {
        return RootView::ReadWrite;
    }
    let mut decision = policy.fs.rules.default_effect;
    let mut access = FsAccess::Read;
    for rule in &policy.fs.rules.entries {
        if rule.matcher.as_str() == "**" {
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

fn append_minimal_read_mounts(
    command: &mut Command,
    policy: &SandboxPolicy,
    spec: &CommandSpec,
    ca_bundle: Option<&Path>,
    bwrap: &Path,
) -> Result<(), Degradation> {
    let DerivedGrants {
        grants,
        read_partial,
    } = linux_grants::derive_read_grants(policy);
    if read_partial {
        return Err(Degradation {
            lost: vec!["fs-read-partial".to_string()],
            reason: Some("read policy exceeded the concrete mount-plan budget".to_string()),
        });
    }
    let mut mounted = BTreeSet::new();
    for dir in ESSENTIAL_READ_DIRS {
        append_ro_mount(command, Path::new(dir), &mut mounted);
    }
    for grant in grants {
        append_ro_mount(command, &grant.path, &mut mounted);
    }
    if let Some(program) = resolve_program(&spec.program, spec.cwd.as_deref()) {
        append_ro_mount(command, &program, &mut mounted);
    }
    if let Some(bundle) = ca_bundle {
        append_ro_mount(command, bundle, &mut mounted);
    }
    // A bundled helper can live outside the platform defaults. Keeping it visible
    // lets a Nub process inside this restrictive view create a stricter child view.
    append_ro_mount(command, bwrap, &mut mounted);
    Ok(())
}

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
    let mut candidates: Vec<(PathBuf, MaskKind)> = Vec::new();
    let mut needs_bounded_snapshot = false;

    for rule in &policy.fs.rules.entries {
        if rule.effect != Effect::Deny {
            continue;
        }
        let pattern = rule.matcher.as_str();
        if let Some(path) = exact_rule_root(pattern) {
            candidates.push((path, MaskKind::Unreadable));
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
        collect_direct_denied_candidates(deny_search_roots, &matcher, &mut candidates);
    }

    let mut seen = HashSet::new();
    let mut masks = Vec::new();
    for (path, kind) in candidates {
        let path = canonicalize_including_nonexistent(&path);
        if !path.exists() || !seen.insert(path.clone()) {
            continue;
        }
        let verdict = matcher.decide(&path);
        if verdict.effect == Effect::Deny {
            masks.push(Mask { path, kind });
        }
    }
    masks.sort_by(|a, b| a.path.cmp(&b.path));
    // If a denied directory is already hidden, child entries need no mounts.
    let dirs: Vec<PathBuf> = masks
        .iter()
        .filter(|m| m.path.is_dir())
        .map(|m| m.path.clone())
        .collect();
    masks.retain(|m| {
        m.path.is_dir()
            || !dirs
                .iter()
                .any(|dir| m.path != *dir && m.path.starts_with(dir))
    });
    Ok(masks)
}

fn collect_direct_denied_candidates(
    roots: &[PathBuf],
    matcher: &PathMatcher,
    out: &mut Vec<(PathBuf, MaskKind)>,
) {
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if matcher.decide(&path).effect != Effect::Deny {
                continue;
            }
            let kind = if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".env"))
            {
                MaskKind::EmptyReadable
            } else {
                MaskKind::Unreadable
            };
            out.push((path, kind));
        }
    }
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
            });
        }
    }
    Ok(masks)
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

fn is_builtin_env_glob(pattern: &str) -> bool {
    matches!(pattern, "**/.env*" | "**/.env*/**" | ".env*" | ".env*/**")
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
    match (metadata.is_dir(), mask.kind) {
        (false, MaskKind::EmptyReadable) => metadata.len() == 0,
        (false, MaskKind::Unreadable) => metadata.permissions().mode() & 0o444 == 0,
        (true, MaskKind::EmptyReadable) => {
            fs::read_dir(&mask.path).is_ok_and(|mut entries| entries.next().is_none())
        }
        (true, MaskKind::Unreadable) => fs::read_dir(&mask.path).is_err(),
    }
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

fn exact_rule_root(pattern: &str) -> Option<PathBuf> {
    let trimmed = pattern.strip_suffix("/**").unwrap_or(pattern);
    if trimmed.contains(['*', '?', '[', '{']) || trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn find_bwrap() -> Result<BubblewrapLaunch, String> {
    // Prefer the distro binary: Ubuntu can grant its known path the AppArmor
    // `userns` permission while rejecting an otherwise identical bundled helper.
    for candidate in [PathBuf::from("/usr/bin/bwrap"), PathBuf::from("/bin/bwrap")] {
        if executable(&candidate) {
            return Ok(BubblewrapLaunch {
                program: candidate.clone(),
                visible_path: candidate,
                executable: None,
            });
        }
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [
            dir.join("nub-resources/bwrap"),
            dir.join("../nub-resources/bwrap"),
            dir.join("bwrap"),
        ] {
            if executable(&candidate) {
                let executable = File::open(&candidate).map_err(|e| {
                    format!("opening bundled Bubblewrap {}: {e}", candidate.display())
                })?;
                verify_bundled_bwrap(&executable, &candidate)?;
                return Ok(BubblewrapLaunch {
                    program: PathBuf::from(format!("/proc/self/fd/{}", executable.as_raw_fd())),
                    visible_path: candidate,
                    executable: Some(executable),
                });
            }
        }
    }
    Err("Bubblewrap helper not found (system and bundled paths checked)".to_string())
}

fn verify_bundled_bwrap(file: &File, path: &Path) -> Result<(), String> {
    let Some(expected) = option_env!("NUB_BWRAP_SHA256") else {
        return Err(format!(
            "bundled Bubblewrap has no build-pinned digest: {}",
            path.display()
        ));
    };
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("build-pinned Bubblewrap digest is malformed".to_string());
    }
    let len = file
        .metadata()
        .map_err(|e| format!("statting bundled Bubblewrap {}: {e}", path.display()))?
        .len();
    if len > 16 * 1024 * 1024 {
        return Err(format!(
            "bundled Bubblewrap is unexpectedly large: {}",
            path.display()
        ));
    }
    let mut bytes = vec![0; len as usize];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|e| format!("reading bundled Bubblewrap {}: {e}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "bundled Bubblewrap digest mismatch for {}",
            path.display()
        ))
    }
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn open_inheritable_dev_null() -> std::io::Result<File> {
    File::open("/dev/null")
}

fn build_network_seccomp() -> Result<BpfProgram, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for network filter: {e}"))?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // The private network namespace handles ordinary IP traffic. AF_UNIX is listed
    // separately because filesystem and abstract Unix sockets cross that boundary.
    // AF_NETLINK remains available so a nested Bubblewrap can configure its own
    // private network namespace; it cannot reach the host network from inside this one.
    let denied_families = [
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
    let mut socket_rules = Vec::with_capacity(denied_families.len());
    for family in denied_families {
        socket_rules.push(
            SeccompRule::new(vec![
                SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, family as u64)
                    .map_err(|e| format!("network-family condition: {e}"))?,
            ])
            .map_err(|e| format!("network-family rule: {e}"))?,
        );
    }
    rules.insert(libc::SYS_socket, socket_rules);

    // io_uring can create sockets without issuing socket(2), so disabling its setup
    // closes the alternate route whenever network access is denied.
    rules.insert(libc::SYS_io_uring_setup, Vec::new());

    SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("building network filter: {e}"))?
    .try_into()
    .map_err(|e| format!("compiling network filter: {e}"))
}

fn write_seccomp_program(program: BpfProgram) -> std::io::Result<File> {
    let mut file = tempfile::tempfile()?;
    let byte_len = program.len() * std::mem::size_of::<libc::sock_filter>();
    let bytes = unsafe { std::slice::from_raw_parts(program.as_ptr().cast::<u8>(), byte_len) };
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

fn seal_inherited_fds(command: &mut Command, bubblewrap_data_fds: Vec<i32>) {
    // Keep only stdio and Bubblewrap's harmless setup-data descriptors. This closes
    // the inherited-open-file escape from path denial while retaining Rust's exec
    // error pipe until exec succeeds. CLOSE_RANGE_CLOEXEC is available since Linux
    // 5.11; an older kernel makes spawn fail instead of weakening the boundary.
    unsafe {
        command.pre_exec(move || {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            let result = libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC);
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for &fd in &bubblewrap_data_fds {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

fn base_command(spec: &CommandSpec, policy: &SandboxPolicy) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    if policy.env.enforce {
        command.env_clear();
        for (key, value) in &policy.env.constructed {
            command.env(key, value);
        }
    }
    command
}

fn pre_create(path: &Path) {
    if !path.exists() {
        let _ = fs::create_dir_all(path);
    }
}

fn resolve_program(program: &OsStr, child_cwd: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    if p.components().count() > 1 {
        let base = child_cwd
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())?;
        let resolved = canonicalize_including_nonexistent(&base.join(p));
        return resolved.exists().then_some(resolved);
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    std::env::split_paths(&path)
        .map(|dir| dir.join(p))
        .find(|candidate| executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::{CompileCtx, compile};
    use crate::matcher::path::Homes;
    use serde_json::json;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn policy(root: &Path, surface: serde_json::Value) -> SandboxPolicy {
        let homes = Homes {
            home: root.join("home"),
            tmp: root.join("tmp"),
            cache: root.join("cache"),
            project: root.join("project"),
        };
        compile(
            &surface,
            &CompileCtx::new(homes, root.join("project"), true, BTreeMap::new()),
        )
        .unwrap()
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
        let p = policy(root.path(), json!({"fs": ["...", "./"]}));
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert_eq!(
            masks,
            vec![Mask {
                path: project.join(".env"),
                kind: MaskKind::EmptyReadable,
            }]
        );
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
            json!({"fs": ["...", "./", format!("!{}", denied.display())]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert!(
            masks
                .iter()
                .any(|m| m.path == denied && m.kind == MaskKind::Unreadable)
        );
    }

    #[test]
    fn direct_sandbox_config_glob_is_unreadable_without_recursive_scan() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("nested")).unwrap();
        fs::write(project.join("tool.sandbox.json"), "SECRET").unwrap();
        fs::write(project.join("nested/ignored.sandbox.json"), "OUT-OF-SCOPE").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["...", "./", "!**/*.sandbox.json"]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        assert_eq!(
            masks,
            vec![Mask {
                path: project.join("tool.sandbox.json"),
                kind: MaskKind::Unreadable,
            }]
        );
    }

    #[test]
    fn nested_parent_glob_is_rejected_instead_of_under_scanned() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join("nested")).unwrap();
        fs::write(project.join("nested/tool.sandbox.json"), "SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["...", "./", "!nested/*.sandbox.json"]}),
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
}
