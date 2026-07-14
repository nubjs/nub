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
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
#[cfg(test)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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

const FIRST_LAUNCH_DATA_FD: RawFd = super::linux_monitor::SIGNAL_RELAY_FD + 1;
const MAX_BUNDLED_BWRAP_BYTES: u64 = 16 * 1024 * 1024;
const REQUIRED_EXECUTABLE_SNAPSHOT_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BubblewrapOrigin {
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
        return Ok(LinuxPreflight { confined: None });
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
    if protects_ambient_credentials(policy) {
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
    validate_reserved_runtime_view(&cwd, &entry_program, &mount_plan, &masks).map_err(
        |reason| Degradation {
            lost: vec!["process-isolation".to_string()],
            reason: Some(reason),
        },
    )?;
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
    // resource exists. The admitted descriptor remains the launch authority.
    let bwrap_candidates = open_bwrap_candidate_inventory();
    if bwrap_candidates.candidates.is_empty() {
        return Err(Degradation {
            lost: vec!["fs".to_string()],
            reason: Some(classify_bwrap_failures(&bwrap_candidates.failures)),
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
    )?;
    Ok(LinuxPreflight {
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

pub fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<File>,
    tmp_dir: Option<&Path>,
    runtime: Option<&super::linux_monitor::RuntimeCapability>,
    preflight: LinuxPreflight,
) -> Result<Prepared, Degradation> {
    let Some(confined) = preflight.confined else {
        return Ok(Prepared {
            command: base_command(&spec, policy),
            degradation: Degradation::full(),
            proxy: None,
            _inherited_files: Vec::new(),
            retained_monitor: None,
            _private_tmp: None,
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
    let target_env = target_environment(policy, proxy_port, proxy_token, ca_bundle.is_some());
    let bootstrap = super::linux_monitor::BootstrapSpec::new(
        runtime,
        policy,
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
        &retained_monitor,
    )?;
    setup_files.extend(effective_ca);
    setup_files.push(bwrap.executable);

    Ok(Prepared {
        command,
        degradation,
        proxy: None,
        _inherited_files: setup_files,
        retained_monitor: Some(retained_monitor),
        _private_tmp: None,
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
    retained_monitor: &super::linux_monitor::RetainedMonitorLaunch,
) -> Result<RetainedOuterSetup, Degradation> {
    let mut setup = Command::new("");
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
            append_minimal_read_mounts(&mut setup, entry_program)?;
        }
    };
    setup.args(["--dev", "/dev"]);
    retained_monitor.append_runtime_mount(&mut setup);
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

    for grant in mount_plan {
        setup
            .arg(match grant.access {
                MountAccess::ReadOnly => "--ro-bind",
                MountAccess::ReadWrite => "--bind",
            })
            .arg(&grant.path)
            .arg(&grant.path);
    }
    let mut mask_sources = Vec::new();
    for mask in masks.iter().filter(|mask| !mask.directory) {
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
    for mask in masks.iter().filter(|mask| mask.directory) {
        setup
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
    // Nub infrastructure is layered after authored masks at a reserved destination
    // below the fresh /dev view. The child never receives the host temporary path,
    // so a Private/Deny /tmp or an ancestor mask cannot hide its trust bundle.
    if let Some(bundle) = ca_bundle {
        setup
            .arg("--ro-bind-fd")
            .arg(bundle.as_raw_fd().to_string())
            .arg(super::linux_monitor::PRIVATE_CA_BUNDLE);
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

    let mut degradation = Degradation::full();
    if policy.net.enforce {
        setup.arg("--unshare-net");
        if proxy_port.is_some() {
            degradation.lost.push("net-per-host".to_string());
            degradation.reason = Some(
                "Bubblewrap proxy bridge not yet wired; network was denied completely".to_string(),
            );
        }
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

#[allow(clippy::too_many_arguments)]
fn admit_bwrap_candidate(
    policy: &SandboxPolicy,
    runtime: &super::linux_monitor::RuntimeCapability,
    root_view: RootView,
    entry_program: &Path,
    mount_plan: &[MountGrant],
    masks: &[Mask],
    inventory: PinnedCandidateInventory,
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
                let mut diagnostic_bytes = Vec::new();
                if let Some(stderr) = stderr.as_mut() {
                    let _ = stderr.read_to_end(&mut diagnostic_bytes);
                }
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
    Err(Degradation {
        lost: vec!["fs".to_string()],
        reason: Some(classify_bwrap_failures(&failures)),
    })
}

fn target_environment(
    policy: &SandboxPolicy,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: bool,
) -> BTreeMap<OsString, OsString> {
    let mut target_env = policy
        .env
        .constructed
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<BTreeMap<_, _>>();
    if let Some(port) = proxy_port {
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
    for dir in ESSENTIAL_READ_DIRS {
        append_ro_mount(command, Path::new(dir), &mut mounted);
    }
    append_ro_mount(command, entry_program, &mut mounted);
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
            reject_denied_hardlink(&logical, &path, &metadata)?;
            masks.push(Mask {
                path,
                kind,
                directory: metadata.is_dir(),
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
        && !ESSENTIAL_READ_DIRS
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

fn validate_process_inputs(spec: &CommandSpec) -> Result<(), String> {
    let reject_nul = |label: &str, value: &OsStr| {
        if value.as_bytes().contains(&0) {
            Err(format!("sandbox {label} contains a NUL byte"))
        } else {
            Ok(())
        }
    };
    reject_nul("entry program", &spec.program)?;
    for (index, arg) in spec.args.iter().enumerate() {
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

fn open_bwrap_candidate_inventory() -> PinnedCandidateInventory {
    let system = [PathBuf::from("/usr/bin/bwrap"), PathBuf::from("/bin/bwrap")];
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
    open_bwrap_candidate_inventory_from(system, bundled)
}

fn open_bwrap_candidate_inventory_from(
    system: impl IntoIterator<Item = PathBuf>,
    bundled: impl IntoIterator<Item = PathBuf>,
) -> PinnedCandidateInventory {
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    for (origin, paths) in [
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
        return Err(format!(
            "opening candidate: {}",
            std::io::Error::last_os_error()
        ));
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
    if origin == BubblewrapOrigin::System
        && (metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0)
    {
        return Err(format!(
            "system candidate is not root-owned and protected from group/other writes (uid={}, mode={:o})",
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
    };
    Ok(PinnedBubblewrapCandidate {
        executable,
        source_path,
        source_identity,
    })
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

fn classify_bwrap_failures(failures: &[String]) -> String {
    let detail = failures.join("; ");
    let lower = detail.to_ascii_lowercase();
    if lower.contains("unknown option") || lower.contains("invalid option") {
        return format!("installed Bubblewrap lacks required stock options ({detail})");
    }
    if fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|value| value.trim() == "1")
        && (lower.contains("permission denied") || lower.contains("uid map"))
    {
        return format!(
            "Bubblewrap is blocked by Ubuntu's AppArmor user-namespace policy ({detail})"
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
    if Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || fs::read_to_string("/proc/1/cgroup").is_ok_and(|value| {
            ["docker", "containerd", "kubepods", "libpod"]
                .iter()
                .any(|marker| value.contains(marker))
        })
    {
        return format!("the container policy blocks required Bubblewrap behavior ({detail})");
    }
    format!("Bubblewrap cannot enforce the required process view ({detail})")
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
    keyctl: i64,
    add_key: i64,
    request_key: i64,
}

pub(super) fn build_seccomp(
    restrict_network: bool,
    deny_keyring: bool,
) -> Result<Option<BpfProgram>, String> {
    if !restrict_network && !deny_keyring {
        return Ok(None);
    }
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for sandbox filter: {e}"))?;
    build_seccomp_for(
        arch,
        restrict_network,
        deny_keyring,
        SandboxSyscalls {
            socket: libc::SYS_socket,
            io_uring_setup: libc::SYS_io_uring_setup,
            keyctl: libc::SYS_keyctl,
            add_key: libc::SYS_add_key,
            request_key: libc::SYS_request_key,
        },
    )
    .map(Some)
}

fn build_seccomp_for(
    arch: TargetArch,
    restrict_network: bool,
    deny_keyring: bool,
    syscalls: SandboxSyscalls,
) -> Result<BpfProgram, String> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    if restrict_network {
        // The private network namespace handles ordinary IP traffic. AF_UNIX is
        // included because filesystem and abstract sockets cross that boundary.
        // AF_NETLINK remains available so nested Bubblewrap can configure its own
        // private network namespace without reaching the host network.
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

        // io_uring can create sockets without issuing socket(2).
        rules.insert(syscalls.io_uring_setup, Vec::new());
    }

    if deny_keyring {
        for syscall in [syscalls.keyctl, syscalls.add_key, syscalls.request_key] {
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

fn base_command(spec: &CommandSpec, policy: &SandboxPolicy) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
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

        let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
        let allowed = u32::from(SeccompAction::Allow);
        let killed = u32::from(SeccompAction::KillProcess);
        let x86 = build_seccomp_for(
            TargetArch::x86_64,
            true,
            true,
            SandboxSyscalls {
                socket: i64::from(X86_64_SOCKET),
                io_uring_setup: i64::from(IO_URING_SETUP),
                keyctl: i64::from(X86_64_KEYCTL),
                add_key: i64::from(X86_64_ADD_KEY),
                request_key: i64::from(X86_64_REQUEST_KEY),
            },
        )
        .unwrap();
        for (syscall, family) in [
            (X86_64_SOCKET, libc::AF_INET),
            (IO_URING_SETUP, 0),
            (X86_64_KEYCTL, 0),
            (X86_64_ADD_KEY, 0),
            (X86_64_REQUEST_KEY, 0),
            (X86_64_SOCKET | X32_SYSCALL_BIT, libc::AF_INET),
            (X86_64_SOCKET | X32_SYSCALL_BIT, libc::AF_NETLINK),
            (IO_URING_SETUP | X32_SYSCALL_BIT, 0),
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
                true,
                SandboxSyscalls {
                    socket: i64::from(GENERIC_SOCKET),
                    io_uring_setup: i64::from(IO_URING_SETUP),
                    keyctl: i64::from(GENERIC_KEYCTL),
                    add_key: i64::from(GENERIC_ADD_KEY),
                    request_key: i64::from(GENERIC_REQUEST_KEY),
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
            assert_eq!(
                evaluate_bpf(&program, &seccomp_data(IO_URING_SETUP, audit_arch, 0)),
                denied,
            );
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
        const KEYCTL: i64 = 250;
        const ADD_KEY: i64 = 248;
        const REQUEST_KEY: i64 = 249;
        const GETPID: u32 = 39;
        let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
        let allowed = u32::from(SeccompAction::Allow);
        let killed = u32::from(SeccompAction::KillProcess);

        assert!(build_seccomp(false, false).unwrap().is_none());
        let build = |network, keyring| {
            build_seccomp_for(
                TargetArch::x86_64,
                network,
                keyring,
                SandboxSyscalls {
                    socket: SOCKET,
                    io_uring_setup: IO_URING_SETUP,
                    keyctl: KEYCTL,
                    add_key: ADD_KEY,
                    request_key: REQUEST_KEY,
                },
            )
            .unwrap()
        };
        let network = build(true, false);
        let keyring = build(false, true);
        let union = build(true, true);

        for syscall in [KEYCTL as u32, ADD_KEY as u32, REQUEST_KEY as u32] {
            assert_eq!(
                evaluate_bpf(&network, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                allowed,
            );
            assert_eq!(
                evaluate_bpf(&keyring, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                denied,
            );
            assert_eq!(
                evaluate_bpf(&union, &seccomp_data(syscall, AUDIT_ARCH_X86_64, 0)),
                denied,
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

        let program = build_seccomp(true, false).unwrap().unwrap();
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
    fn denied_regular_file_with_hardlink_alias_fails_closed() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let denied = project.join("secret.txt");
        fs::write(&denied, "SECRET").unwrap();
        fs::hard_link(&denied, project.join("alias.txt")).unwrap();
        let p = policy(
            root.path(),
            json!({"fs": ["...", "./", format!("!{}", denied.display())]}),
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
        let p = policy(root.path(), json!({"fs": ["...", "./"]}));

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
            json!({"fs": ["...", "./", format!("!{}", denied.display())]}),
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
        };
        assert!(validate_entry_visibility(&entry, TmpMode::Shared, &[mask], &[]).is_err());
        assert!(validate_entry_visibility(&entry, TmpMode::Deny, &[], &[]).is_err());
        let grant = linux_grants::MountGrant {
            path: PathBuf::from("/tmp/project"),
            access: MountAccess::ReadOnly,
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
        };
        assert!(validate_reserved_runtime_view(Path::new("/"), entry, &[grant], &[]).is_err());
        let mask = Mask {
            path: root.join("lib"),
            kind: MaskKind::Unreadable,
            directory: true,
        };
        assert!(validate_reserved_runtime_view(Path::new("/"), entry, &[], &[mask]).is_err());
    }

    #[test]
    fn bwrap_failure_diagnostics_are_host_specific_without_admin_advice() {
        let message = classify_bwrap_failures(&["candidate: unknown option --info-fd".into()]);
        assert!(message.contains("required stock options"), "{message}");
        for banned in ["sudo", "sysctl", "disable AppArmor", "apparmor_parser"] {
            assert!(!message.contains(banned), "{message}");
        }
    }
}
