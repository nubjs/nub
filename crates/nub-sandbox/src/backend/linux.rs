//! Linux backend built around Bubblewrap's private filesystem and PID views.
//!
//! Bubblewrap constructs the view; it does not copy project files. Read-only and
//! writable binds keep their original absolute paths and write directly to the host.
//! Exact deny paths are layered last so a writable project cannot re-expose them.
#![cfg(target_os = "linux")]

use crate::backend::linux_grants::{self, MountAccess, fs_confines};
use crate::backend::{CommandSpec, Degradation, Prepared};
use crate::matcher::path::{PathMatcher, canonicalize_including_nonexistent};
use crate::policy::{Effect, FsAccess, SandboxPolicy, TmpMode};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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
    directory: bool,
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
    let cwd = fs::canonicalize(&cwd).map_err(|e| Degradation {
        lost: vec!["fs".to_string()],
        reason: Some(format!(
            "resolving sandbox working directory {}: {e}",
            cwd.display()
        )),
    })?;
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
    // Rebinding an inherited read-only filesystem as writable cannot widen it on
    // Linux, but recording the effective cap makes the argv plan explicit and
    // prevents later launcher changes from accidentally claiming otherwise.
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
    masks.extend(alternate_procfs_masks().map_err(|reason| Degradation {
        lost: vec!["proc".to_string()],
        reason: Some(reason),
    })?);
    masks = merge_masks(masks);
    validate_masks_against_mount_plan(policy, &masks, &mount_plan).map_err(|reason| {
        Degradation {
            lost: vec!["fs-read-deny".to_string()],
            reason: Some(reason),
        }
    })?;
    validate_entry_visibility(&entry_program, policy.fs.tmp, &masks, &mount_plan).map_err(
        |reason| Degradation {
            lost: vec!["process-entry".to_string()],
            reason: Some(reason),
        },
    )?;
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
                &entry_program,
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

    for grant in &mount_plan {
        command
            .arg(match grant.access {
                MountAccess::ReadOnly => "--ro-bind",
                MountAccess::ReadWrite => "--bind",
            })
            .arg(&grant.path)
            .arg(&grant.path);
    }

    let mut mask_sources = Vec::new();
    for mask in masks.iter().filter(|m| !m.directory) {
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
    for mask in masks.iter().filter(|m| m.directory) {
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
    if root_view == RootView::Minimal {
        // Bubblewrap creates destination ancestors in its synthetic root. Freeze
        // them after every authored mount and mask has landed so they do not become
        // accidental write grants; explicit writable submounts retain their flags.
        command.args(["--remount-ro", "/"]);
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
    command.arg(&entry_program).args(&spec.args);

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

fn append_minimal_read_mounts(
    command: &mut Command,
    entry_program: &Path,
    ca_bundle: Option<&Path>,
    bwrap: &Path,
) -> Result<(), Degradation> {
    let mut mounted = BTreeSet::new();
    for dir in ESSENTIAL_READ_DIRS {
        append_ro_mount(command, Path::new(dir), &mut mounted);
    }
    append_ro_mount(command, entry_program, &mut mounted);
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
    let mut candidates: Vec<(PathBuf, PathBuf, MaskKind, bool)> = Vec::new();
    let mut needs_bounded_snapshot = false;

    for rule in &policy.fs.rules.entries {
        if rule.effect != Effect::Deny {
            continue;
        }
        let pattern = rule.matcher.as_str();
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
            masks.push(Mask {
                path,
                kind,
                directory: metadata.is_dir(),
            });
        }
    }
    Ok(merge_masks(masks))
}

fn collect_direct_denied_candidates(
    policy: &SandboxPolicy,
    roots: &[PathBuf],
    matcher: &PathMatcher,
    out: &mut Vec<(PathBuf, PathBuf, MaskKind, bool)>,
) -> Result<(), String> {
    let roots = strict_search_roots(roots)?;
    for root in roots {
        let entries = fs::read_dir(&root)
            .map_err(|e| format!("enumerating deny-search root {}: {e}", root.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|e| format!("enumerating deny-search root {}: {e}", root.display()))?;
            let logical = root.join(entry.file_name());
            let path = entry.path();
            match fs::metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "statting deny candidate {}: {error}",
                        path.display()
                    ));
                }
            }
            let resolved = fs::canonicalize(&path)
                .map_err(|e| format!("resolving deny candidate {}: {e}", path.display()))?;
            if !matcher.matches_deny_entry(&logical, &resolved) {
                continue;
            }
            if matcher
                .decide_logical_or_resolved(&logical, &resolved)
                .effect
                != Effect::Deny
            {
                continue;
            }
            let dotenv_name = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".env"));
            let explicit_user_deny = builtin_env_band_start(policy).is_some_and(|end| {
                matcher.last_matching_effect_before(&logical, &resolved, end) == Some(Effect::Deny)
            });
            let kind = if dotenv_name && !explicit_user_deny {
                MaskKind::EmptyReadable
            } else {
                MaskKind::Unreadable
            };
            out.push((logical, resolved, kind, false));
        }
    }
    Ok(())
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

fn builtin_env_band_start(policy: &SandboxPolicy) -> Option<usize> {
    let entries = &policy.fs.rules.entries;
    if entries.len() < 4
        || entries[entries.len() - 2].matcher.as_str() != "**/.env*/**"
        || entries[entries.len() - 1].matcher.as_str() != ".env*/**"
    {
        return None;
    }
    (0..entries.len() - 2).rev().find(|&index| {
        index + 1 < entries.len() - 2
            && entries[index].effect == Effect::Deny
            && entries[index].matcher.as_str() == "**/.env*"
            && entries[index + 1].effect == Effect::Deny
            && entries[index + 1].matcher.as_str() == ".env*"
    })
}

fn validate_masks_against_mount_plan(
    policy: &SandboxPolicy,
    masks: &[Mask],
    grants: &[linux_grants::MountGrant],
) -> Result<(), String> {
    let matcher = PathMatcher::new(&policy.fs.rules);
    for mask in masks.iter().filter(|mask| mask.directory) {
        for grant in grants
            .iter()
            .filter(|grant| grant.path != mask.path && grant.path.starts_with(&mask.path))
        {
            if matcher.decide(&grant.path).effect == Effect::Allow {
                return Err(format!(
                    "denied directory {} contains a later allowed mount {}; stock Bubblewrap cannot preserve that ordering",
                    mask.path.display(),
                    grant.path.display()
                ));
            }
        }
    }
    Ok(())
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

fn target_path(policy: &SandboxPolicy) -> Option<OsString> {
    target_path_with(policy, std::env::var_os("PATH"))
}

fn target_path_with(policy: &SandboxPolicy, ambient: Option<OsString>) -> Option<OsString> {
    if policy.env.enforce {
        return policy.env.constructed.get("PATH").map(OsString::from);
    }
    Some(ambient.unwrap_or_else(|| OsString::from("/usr/bin:/bin")))
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
                directory: false,
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
    fn explicit_dotenv_deny_is_unreadable_not_an_empty_readable_default() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let denied = project.join(".env");
        fs::write(&denied, "SECRET").unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["...", "./", format!("!{}", denied.display())]}),
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

        let default_policy = policy(root.path(), json!({"fs": ["...", "./"]}));
        let masks = collect_masks(&default_policy, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].path, fs::canonicalize(&target).unwrap());
        assert_eq!(masks[0].kind, MaskKind::EmptyReadable);

        let explicit = policy(
            root.path(),
            json!({"fs": ["...", "./", format!("!{}", target.display())]}),
        );
        let masks = collect_masks(&explicit, std::slice::from_ref(&project)).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].path, fs::canonicalize(&target).unwrap());
        assert_eq!(masks[0].kind, MaskKind::Unreadable);
    }

    #[test]
    fn deny_search_roots_are_strict_and_exact_absence_is_skipped() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let p = policy(root.path(), json!({"fs": ["...", "./"]}));
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
            },
            Mask {
                path: PathBuf::from("/z"),
                kind: MaskKind::EmptyReadable,
                directory: false,
            },
            Mask {
                path: path.clone(),
                kind: MaskKind::Unreadable,
                directory: false,
            },
        ]);
        assert_eq!(masks[0].path, path);
        assert_eq!(masks[0].kind, MaskKind::Unreadable);
        assert_eq!(masks[1].path, PathBuf::from("/z"));
    }

    #[test]
    fn denied_directory_with_later_allowed_child_is_rejected() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        let denied = project.join("denied");
        let child = denied.join("child");
        fs::create_dir_all(&child).unwrap();
        let p = policy(
            root.path(),
            json!({"fs": [
                "...",
                format!("!{}", denied.display()),
                child.to_string_lossy().to_string()
            ]}),
        );
        let masks = collect_masks(&p, std::slice::from_ref(&project)).unwrap();
        let plan = linux_grants::compile_mount_plan(&p).unwrap();
        let error = validate_masks_against_mount_plan(&p, &masks, &plan).unwrap_err();
        assert!(error.contains("later allowed mount"), "{error}");
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
                "...",
                format!("!{}/**", denied.display()),
                denied.to_string_lossy().to_string()
            ]}),
        );
        let error = collect_masks(&p, std::slice::from_ref(&project)).unwrap_err();
        assert!(error.contains("exact directory allow"), "{error}");
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
                directory: false,
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

    #[test]
    fn inherited_read_only_mount_caps_a_writable_request() {
        let mut plan = vec![linux_grants::MountGrant {
            path: PathBuf::from("/inherited/read-only"),
            access: MountAccess::ReadWrite,
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
        assert_eq!(
            target_path_with(&enforced, Some(OsString::from("/wrong"))),
            Some(OsString::from("target-bin"))
        );
        assert_eq!(
            resolve_program(OsStr::new("tool"), &cwd, target_path(&enforced).as_deref()),
            Some(fs::canonicalize(&tool).unwrap())
        );

        let inherited = SandboxPolicy::default();
        assert_eq!(
            target_path_with(&inherited, Some(OsString::from("/ambient"))),
            Some(OsString::from("/ambient"))
        );

        let mut missing = enforced.clone();
        missing.env.constructed.remove("PATH");
        assert_eq!(
            target_path_with(&missing, Some(OsString::from("/ambient"))),
            None
        );
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
        };
        assert!(validate_entry_visibility(&entry, TmpMode::Shared, &[mask], &[]).is_err());
        assert!(validate_entry_visibility(&entry, TmpMode::Deny, &[], &[]).is_err());
        let grant = linux_grants::MountGrant {
            path: PathBuf::from("/tmp/project"),
            access: MountAccess::ReadOnly,
        };
        assert!(validate_entry_visibility(&entry, TmpMode::Deny, &[], &[grant]).is_ok());
    }
}
