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
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

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

pub(crate) struct LinuxSupervision {
    info_read: File,
    info_write: Option<File>,
    block_read: Option<File>,
    block_write: File,
    expected: ExpectedView,
}

struct ExpectedView {
    user: PathBuf,
    mount: PathBuf,
    pid: PathBuf,
    ipc: PathBuf,
    net: Option<PathBuf>,
    require_seccomp: bool,
}

#[derive(serde::Deserialize)]
struct BubblewrapInfo {
    #[serde(rename = "child-pid")]
    child_pid: i32,
}

impl ExpectedView {
    fn capture(require_net: bool, require_seccomp: bool) -> std::io::Result<Self> {
        let namespace = |name: &str| fs::read_link(format!("/proc/self/ns/{name}"));
        Ok(Self {
            user: namespace("user")?,
            mount: namespace("mnt")?,
            pid: namespace("pid")?,
            ipc: namespace("ipc")?,
            net: require_net.then(|| namespace("net")).transpose()?,
            require_seccomp,
        })
    }
}

impl LinuxSupervision {
    fn new(require_net: bool, require_seccomp: bool) -> std::io::Result<Self> {
        let (info_read, info_write) = pipe_pair()?;
        set_nonblocking(&info_read)?;
        let (block_read, block_write) = pipe_pair()?;
        Ok(Self {
            info_read,
            info_write: Some(info_write),
            block_read: Some(block_read),
            block_write,
            expected: ExpectedView::capture(require_net, require_seccomp)?,
        })
    }

    fn append_args(&self, setup: &mut Command) {
        setup
            .arg("--info-fd")
            .arg(
                self.info_write
                    .as_ref()
                    .expect("supervision info writer available")
                    .as_raw_fd()
                    .to_string(),
            )
            .arg("--block-fd")
            .arg(
                self.block_read
                    .as_ref()
                    .expect("supervision block reader available")
                    .as_raw_fd()
                    .to_string(),
            );
    }

    fn child_fds(&self) -> [RawFd; 2] {
        [
            self.info_write
                .as_ref()
                .expect("supervision info writer available")
                .as_raw_fd(),
            self.block_read
                .as_ref()
                .expect("supervision block reader available")
                .as_raw_fd(),
        ]
    }

    pub(crate) fn verify_and_release(mut self, child: &mut Child) -> std::io::Result<i32> {
        let reaper_pid = self.verify(child)?;
        let target_pid = self.release_and_verify(child, reaper_pid)?;
        self.resume(target_pid)?;
        Ok(target_pid)
    }

    pub(crate) fn verify(&mut self, child: &mut Child) -> std::io::Result<i32> {
        self.info_write.take();
        self.block_read.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = Vec::with_capacity(512);
        let info = loop {
            let mut chunk = [0u8; 512];
            match self.info_read.read(&mut chunk) {
                Ok(0) => {
                    if let Ok(info) = serde_json::from_slice::<BubblewrapInfo>(&bytes) {
                        break info;
                    }
                    return Err(std::io::Error::other(
                        "Bubblewrap closed its info channel before a complete status record",
                    ));
                }
                Ok(count) => {
                    bytes.extend_from_slice(&chunk[..count]);
                    if bytes.len() > 64 * 1024 {
                        return Err(std::io::Error::other(
                            "Bubblewrap returned an oversized status record",
                        ));
                    }
                    if let Ok(info) = serde_json::from_slice::<BubblewrapInfo>(&bytes) {
                        break info;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if let Some(status) = child.try_wait()? {
                return Err(std::io::Error::other(format!(
                    "Bubblewrap exited before its supervised child was ready: {status}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for Bubblewrap's supervised child",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        verify_child_view(info.child_pid, child.id(), &self.expected)?;
        Ok(info.child_pid)
    }

    pub(crate) fn release(&mut self) -> std::io::Result<()> {
        self.block_write.write_all(b"1")?;
        Ok(())
    }

    pub(crate) fn release_and_verify(
        &mut self,
        child: &mut Child,
        reaper_pid: i32,
    ) -> std::io::Result<i32> {
        self.release()?;
        await_hardened_child(reaper_pid, child, &self.expected)
    }

    pub(crate) fn resume(&self, target_pid: i32) -> std::io::Result<()> {
        if unsafe { libc::kill(target_pid, libc::SIGCONT) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn set_nonblocking(file: &File) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn pipe_pair() -> std::io::Result<(File, File)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let first = file_above_stdio(unsafe { File::from_raw_fd(fds[0]) })?;
    let second = file_above_stdio(unsafe { File::from_raw_fd(fds[1]) })?;
    Ok((first, second))
}

fn verify_child_view(pid: i32, outer_pid: u32, expected: &ExpectedView) -> std::io::Result<()> {
    if pid <= 0 {
        return Err(std::io::Error::other(
            "Bubblewrap reported an invalid supervised child PID",
        ));
    }
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
    };
    let parent = field("PPid:")
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| std::io::Error::other("supervised child status omitted PPid"))?;
    if parent != outer_pid {
        return Err(std::io::Error::other(
            "Bubblewrap status identified a process outside its launcher tree",
        ));
    }
    let current = |name: &str| fs::read_link(format!("/proc/{pid}/ns/{name}"));
    for (name, outer) in [
        ("user", &expected.user),
        ("mnt", &expected.mount),
        ("pid", &expected.pid),
        ("ipc", &expected.ipc),
    ] {
        if current(name)? == *outer {
            return Err(std::io::Error::other(format!(
                "supervised child did not enter a distinct {name} namespace"
            )));
        }
    }
    if let Some(outer_net) = &expected.net
        && current("net")? == *outer_net
    {
        return Err(std::io::Error::other(
            "supervised child did not enter the requested network namespace",
        ));
    }
    Ok(())
}

fn await_hardened_child(
    reaper_pid: i32,
    child: &mut Child,
    expected: &ExpectedView,
) -> std::io::Result<i32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(target_pid) = single_child_pid(reaper_pid)? {
            let status = fs::read_to_string(format!("/proc/{target_pid}/status"));
            if status.as_deref().is_ok_and(|status| {
                status.lines().any(|line| {
                    line.strip_prefix("State:")
                        .is_some_and(|state| state.trim().starts_with(['T', 't']))
                })
            }) {
                verify_hardened_child(target_pid, reaper_pid, expected)?;
                return Ok(target_pid);
            }
        }
        if let Some(status) = child.try_wait()? {
            return Err(std::io::Error::other(format!(
                "Bubblewrap exited before its target hardening was verified: {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for the sandbox target verification stop",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn single_child_pid(reaper_pid: i32) -> std::io::Result<Option<i32>> {
    let children = fs::read_to_string(format!("/proc/{reaper_pid}/task/{reaper_pid}/children"))?;
    let children = children
        .split_whitespace()
        .map(str::parse::<i32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| std::io::Error::other("Bubblewrap reported an invalid target PID"))?;
    match children.as_slice() {
        [] => Ok(None),
        [pid] if *pid > 0 => Ok(Some(*pid)),
        _ => Err(std::io::Error::other(format!(
            "Bubblewrap's namespace reaper had {} children at the verification gate",
            children.len()
        ))),
    }
}

fn verify_hardened_child(
    pid: i32,
    reaper_pid: i32,
    expected: &ExpectedView,
) -> std::io::Result<()> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
    };
    if !field("State:").is_some_and(|state| state.starts_with(['T', 't'])) {
        return Err(std::io::Error::other(
            "sandbox target did not stop at its verification gate",
        ));
    }
    if field("PPid:").and_then(|value| value.parse().ok()) != Some(reaper_pid) {
        return Err(std::io::Error::other(
            "the sandbox target was not owned by Bubblewrap's namespace reaper",
        ));
    }
    for capability in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        if field(capability) != Some("0000000000000000") {
            return Err(std::io::Error::other(format!(
                "sandbox target retained Linux capabilities in {capability}"
            )));
        }
    }
    if expected.require_seccomp && field("Seccomp:") != Some("2") {
        return Err(std::io::Error::other(
            "sandbox target did not enter seccomp filter mode",
        ));
    }
    let sid = unsafe { libc::getsid(pid) };
    let pgid = unsafe { libc::getpgid(pid) };
    if sid != pid || pgid != pid {
        return Err(std::io::Error::other(format!(
            "sandbox target is not its verified session-group leader (pid={pid}, sid={sid}, pgid={pgid})"
        )));
    }
    for name in ["user", "mnt", "pid", "ipc"] {
        if fs::read_link(format!("/proc/{pid}/ns/{name}"))?
            != fs::read_link(format!("/proc/{reaper_pid}/ns/{name}"))?
        {
            return Err(std::io::Error::other(format!(
                "sandbox target escaped Bubblewrap's {name} namespace"
            )));
        }
    }
    if expected.net.is_some()
        && fs::read_link(format!("/proc/{pid}/ns/net"))?
            != fs::read_link(format!("/proc/{reaper_pid}/ns/net"))?
    {
        return Err(std::io::Error::other(
            "sandbox target escaped Bubblewrap's network namespace",
        ));
    }
    Ok(())
}

/// Apply a resolved policy using Bubblewrap. The exact same operation can be nested:
/// an outer mount/PID view remains in force and the child adds a stricter view inside it.
pub(crate) fn preflight_process(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
) -> Result<(), Degradation> {
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
    resolve_program(&spec.program, &cwd, target_path(policy).as_deref()).ok_or_else(|| {
        Degradation {
            lost: vec!["process-entry".to_string()],
            reason: Some(format!(
                "sandbox entry program could not be resolved in the target environment: {}",
                Path::new(&spec.program).display()
            )),
        }
    })?;
    Ok(())
}

pub fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&Path>,
    tmp_dir: Option<&Path>,
    runtime: Option<&super::linux_monitor::RuntimeCapability>,
) -> Result<Prepared, Degradation> {
    validate_process_inputs(&spec).map_err(|reason| Degradation {
        lost: vec!["process-input".to_string()],
        reason: Some(reason),
    })?;
    let confine_fs = fs_confines(&policy.fs);
    let sandboxing =
        confine_fs || policy.net.enforce || policy.env.enforce || policy.fs.tmp != TmpMode::Shared;

    if !sandboxing {
        return Ok(Prepared {
            command: base_command(&spec, policy),
            degradation: Degradation::full(),
            proxy: None,
            _inherited_files: Vec::new(),
            supervision: None,
            _private_tmp: None,
        });
    }

    let runtime = runtime.ok_or_else(|| Degradation {
        lost: vec!["runtime-capability-missing".to_string()],
        reason: Some(
            "Linux sandbox confinement requires the embedder's earliest bootstrap capability"
                .to_string(),
        ),
    })?;
    let _runtime = runtime
        .materialize()
        .map_err(super::linux_monitor::runtime_degradation)?;

    let (bwrap, setsid_program) = find_bwrap(policy.net.enforce).map_err(|reason| Degradation {
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
    validate_cwd_visibility(&cwd, root_view, policy.fs.tmp, &masks, &mount_plan).map_err(
        |reason| Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some(reason),
        },
    )?;
    let masks = masks
        .into_iter()
        .filter(|mask| !mask_already_enforced(mask))
        .collect::<Vec<_>>();

    let mut setup = Command::new("");
    setup.args(["--die-with-parent", "--new-session", "--unshare-user"]);
    setup.args(["--cap-drop", "ALL"]);
    // Deliberately do not disable further user namespaces: a sandboxed agent must be
    // able to invoke Nub again and add a stricter child sandbox.
    setup.args(["--unshare-pid", "--unshare-ipc"]);

    match root_view {
        RootView::ReadWrite => {
            setup.args(["--bind", "/", "/"]);
        }
        RootView::ReadOnly => {
            setup.args(["--ro-bind", "/", "/"]);
        }
        RootView::Minimal => {
            append_minimal_read_mounts(&mut setup, &entry_program, ca_bundle, &bwrap.visible_path)?;
        }
    };

    // Replace host devices and host process information immediately after the root
    // view. Policy masks are layered later, so an explicit deny below `/dev` or the
    // fresh `/proc` cannot be hidden by these ancestor mounts.
    setup.args(["--dev", "/dev", "--proc", "/proc"]);

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
            // Traverse-only lets a later explicit project bind under `/tmp` remain
            // reachable, while the empty read-only tmp itself cannot be listed or
            // written.
            setup.args(["--perms", "111", "--tmpfs", "/tmp"]);
        }
    }

    for grant in &mount_plan {
        setup
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
        setup
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
    if policy.fs.tmp == TmpMode::Deny {
        // Delay this until child mounts under `/tmp` have been installed; otherwise
        // Bubblewrap cannot create their destination mountpoints.
        setup.args(["--remount-ro", "/tmp"]);
    }
    if root_view == RootView::Minimal {
        // Bubblewrap creates destination ancestors in its synthetic root. Freeze
        // them after every authored mount and mask has landed so they do not become
        // accidental write grants; explicit writable submounts retain their flags.
        setup.args(["--remount-ro", "/"]);
    }

    let mut degradation = Degradation::full();
    if policy.net.enforce {
        // A route-less network namespace is the fail-safe floor. The follow-up bridge
        // will reconnect only the already-running, host-side filtered proxy.
        setup.arg("--unshare-net");
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
        setup.arg("--seccomp").arg(source.as_raw_fd().to_string());
        Some(source)
    } else {
        None
    };

    let supervision =
        LinuxSupervision::new(policy.net.enforce, seccomp_source.is_some()).map_err(|error| {
            Degradation {
                lost: vec!["process-isolation".to_string()],
                reason: Some(format!("preparing Bubblewrap supervision: {error}")),
            }
        })?;
    supervision.append_args(&mut setup);

    let mut target_env = policy
        .env
        .constructed
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<BTreeMap<_, _>>();
    if let Some(port) = proxy_port {
        super::insert_proxy_env(&mut target_env, port, proxy_token);
    }
    if let Some(bundle) = ca_bundle {
        super::insert_ca_env(&mut target_env, bundle);
    }
    if policy.fs.tmp == TmpMode::Private {
        super::insert_tmp_env(&mut target_env, Path::new("/tmp"));
    }
    append_target_environment(&mut setup, &target_env).map_err(|reason| Degradation {
        lost: vec!["env".to_string()],
        reason: Some(reason),
    })?;
    setup.arg("--chdir").arg(&cwd);

    let arguments = write_bwrap_arguments(setup.get_args()).map_err(|e| Degradation {
        lost: vec!["process-entry".to_string()],
        reason: Some(format!("serializing Bubblewrap launch arguments: {e}")),
    })?;
    let mut command = Command::new(&bwrap.program);
    command.env_clear();
    command
        .arg("--args")
        .arg(arguments.as_raw_fd().to_string())
        .arg("--")
        .arg(&setsid_program)
        .arg("/bin/sh")
        .args(["-c", TARGET_GATE_SCRIPT, "nub-sandbox-target"])
        .arg(&entry_program)
        .args(&spec.args);
    let inherited_fds = mask_sources
        .iter()
        .chain(seccomp_source.iter())
        .chain(std::iter::once(&arguments))
        .chain(bwrap.executable.iter())
        .map(AsRawFd::as_raw_fd)
        .chain(supervision.child_fds())
        .collect::<Vec<_>>();
    seal_inherited_fds(&mut command, inherited_fds);

    let mut inherited_files = Vec::with_capacity(2);
    inherited_files.extend(mask_sources);
    inherited_files.extend(seccomp_source);
    inherited_files.push(arguments);
    inherited_files.extend(bwrap.executable);

    Ok(Prepared {
        command,
        degradation,
        proxy: None,
        _inherited_files: inherited_files,
        supervision: Some(supervision),
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

#[derive(Clone)]
enum BubblewrapCandidate {
    System(PathBuf),
    Bundled(PathBuf),
}

fn find_bwrap(require_net: bool) -> Result<(BubblewrapLaunch, PathBuf), String> {
    let setsid_program = find_setsid()?;
    let candidate = choose_bwrap(require_net, &setsid_program)?;
    Ok((launch_bwrap(&candidate)?, setsid_program))
}

fn choose_bwrap(require_net: bool, setsid_program: &Path) -> Result<BubblewrapCandidate, String> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for path in [PathBuf::from("/usr/bin/bwrap"), PathBuf::from("/bin/bwrap")] {
        if executable(&path)
            && let Ok(canonical) = fs::canonicalize(path)
            && seen.insert(canonical.clone())
        {
            candidates.push(BubblewrapCandidate::System(canonical));
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for path in [
            dir.join("nub-resources/bwrap"),
            dir.join("../nub-resources/bwrap"),
            dir.join("bwrap"),
        ] {
            if executable(&path)
                && let Ok(canonical) = fs::canonicalize(path)
                && seen.insert(canonical.clone())
            {
                candidates.push(BubblewrapCandidate::Bundled(canonical));
            }
        }
    }
    if candidates.is_empty() {
        return Err("Bubblewrap helper not found (system and bundled paths checked)".to_string());
    }
    let mut failures = Vec::new();
    for candidate in candidates {
        match launch_bwrap(&candidate).and_then(|launch| {
            probe_bwrap(&launch, setsid_program, require_net)?;
            Ok(launch)
        }) {
            Ok(_) => return Ok(candidate),
            Err(reason) => failures.push(format!("{}: {reason}", candidate.path().display())),
        }
    }
    Err(classify_bwrap_failures(&failures))
}

fn find_setsid() -> Result<PathBuf, String> {
    find_setsid_in([
        PathBuf::from("/usr/bin/setsid"),
        PathBuf::from("/bin/setsid"),
    ])
}

fn find_setsid_in(paths: impl IntoIterator<Item = PathBuf>) -> Result<PathBuf, String> {
    paths
        .into_iter()
        .find_map(|path| {
            executable(&path)
                .then(|| fs::canonicalize(path).ok())
                .flatten()
        })
        .ok_or_else(|| {
            "the stock setsid launcher required for supervised sessions was not found".to_string()
        })
}

impl BubblewrapCandidate {
    fn path(&self) -> &Path {
        match self {
            Self::System(path) | Self::Bundled(path) => path,
        }
    }
}

fn launch_bwrap(candidate: &BubblewrapCandidate) -> Result<BubblewrapLaunch, String> {
    match candidate {
        BubblewrapCandidate::System(path) => Ok(BubblewrapLaunch {
            program: path.clone(),
            visible_path: path.clone(),
            executable: None,
        }),
        BubblewrapCandidate::Bundled(path) => {
            let executable = file_above_stdio(
                File::open(path).map_err(|error| format!("opening bundled Bubblewrap: {error}"))?,
            )
            .map_err(|error| format!("duplicating bundled Bubblewrap: {error}"))?;
            verify_bundled_bwrap(&executable, path)?;
            Ok(BubblewrapLaunch {
                program: PathBuf::from(format!("/proc/self/fd/{}", executable.as_raw_fd())),
                visible_path: path.clone(),
                executable: Some(executable),
            })
        }
    }
}

fn classify_bwrap_failures(failures: &[String]) -> String {
    let detail = failures.join("; ");
    let lower = detail.to_ascii_lowercase();
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
    if lower.contains("unknown option") || lower.contains("invalid option") {
        return format!("installed Bubblewrap lacks required stock options ({detail})");
    }
    format!("Bubblewrap cannot enforce the required process view ({detail})")
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

fn probe_bwrap(
    launch: &BubblewrapLaunch,
    setsid_program: &Path,
    require_net: bool,
) -> Result<(), String> {
    let destination = tempfile::Builder::new()
        .prefix("nub-bwrap-probe-mask-")
        .tempfile_in("/var/tmp")
        .map_err(|error| format!("creating mask destination: {error}"))?;
    fs::write(destination.path(), b"host-bytes")
        .map_err(|error| format!("seeding mask destination: {error}"))?;
    let source = open_inheritable_dev_null()
        .map_err(|error| format!("opening probe mask source: {error}"))?;
    let seccomp = if require_net {
        Some(
            write_seccomp_program(build_probe_seccomp()?)
                .map_err(|error| format!("writing probe seccomp program: {error}"))?,
        )
    } else {
        None
    };
    let supervision = LinuxSupervision::new(require_net, require_net)
        .map_err(|error| format!("preparing probe supervision: {error}"))?;

    let mut setup = Command::new("");
    setup.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--cap-drop",
        "ALL",
        "--unshare-pid",
        "--unshare-ipc",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--remount-ro",
        "/tmp",
        "--perms",
        "000",
        "--ro-bind-data",
    ]);
    setup
        .arg(source.as_raw_fd().to_string())
        .arg(destination.path());
    if require_net {
        setup.arg("--unshare-net");
        setup
            .arg("--seccomp")
            .arg(seccomp.as_ref().unwrap().as_raw_fd().to_string());
    }
    supervision.append_args(&mut setup);
    setup.args(["--clearenv", "--setenv", "PATH", "/usr/bin:/bin"]);
    let arguments = write_bwrap_arguments(setup.get_args())
        .map_err(|error| format!("serializing probe arguments: {error}"))?;
    let mut command = Command::new(&launch.program);
    command
        .env_clear()
        .arg("--args")
        .arg(arguments.as_raw_fd().to_string())
        .arg("--")
        .arg(setsid_program)
        .arg("/bin/sh")
        .args(["-c", TARGET_GATE_SCRIPT, "nub-bwrap-target"])
        .arg("/bin/sh")
        .args(["-c", BWRAP_PROBE_SCRIPT, "nub-bwrap-probe"])
        .arg(destination.path())
        .arg(if require_net { "1" } else { "0" })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let child_fds = [source.as_raw_fd(), arguments.as_raw_fd()]
        .into_iter()
        .chain(seccomp.iter().map(AsRawFd::as_raw_fd))
        .chain(launch.executable.iter().map(AsRawFd::as_raw_fd))
        .chain(supervision.child_fds())
        .collect::<Vec<_>>();
    seal_inherited_fds(&mut command, child_fds);
    let mut child = command
        .spawn()
        .map_err(|error| format!("probe could not start: {error}"))?;
    let pgid = match supervision.verify_and_release(&mut child) {
        Ok(pgid) => pgid,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            return Err(format!(
                "probe supervision failed: {error}{}",
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            ));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("waiting for probe: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
            return Err("probe timed out".to_string());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("behavior probe exited with {status}"))
    } else {
        Err(format!("behavior probe exited with {status}: {stderr}"))
    }
}

const BWRAP_PROBE_SCRIPT: &str = r#"
set -eu
[ ! -s "$1" ]
[ "$(stat -c %a "$1")" = 0 ]
if (printf x > "$1") 2>/dev/null; then exit 21; fi
if (printf x > /tmp/nub-bwrap-probe-write) 2>/dev/null; then exit 22; fi
[ -c /dev/null ]
[ -r /proc/self/status ]
if [ "$2" = 1 ]; then
  routes=0
  while read -r line; do
    case "$line" in Iface*) continue ;; '') continue ;; *) routes=1 ;; esac
  done < /proc/net/route
  [ "$routes" = 0 ]
  if /usr/bin/uname >/dev/null 2>&1; then exit 23; fi
fi
"#;

const TARGET_GATE_SCRIPT: &str = r#"kill -STOP $$ || exit 125
exec "$@"
"#;

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

fn append_target_environment(
    setup: &mut Command,
    env: &BTreeMap<OsString, OsString>,
) -> Result<(), String> {
    setup.arg("--clearenv");
    for (key, value) in env {
        let key_bytes = key.as_bytes();
        if key_bytes.is_empty() || key_bytes.contains(&b'=') || key_bytes.contains(&0) {
            return Err(format!("invalid target environment key: {key:?}"));
        }
        if value.as_bytes().contains(&0) {
            return Err(format!(
                "target environment variable {key:?} contains a NUL byte"
            ));
        }
        setup.arg("--setenv").arg(key).arg(value);
    }
    Ok(())
}

fn write_bwrap_arguments<'a>(args: impl Iterator<Item = &'a OsStr>) -> std::io::Result<File> {
    let mut file = file_above_stdio(tempfile::tempfile()?)?;
    for arg in args {
        if arg.as_bytes().contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a Bubblewrap argument contains a NUL byte",
            ));
        }
        file.write_all(arg.as_bytes())?;
        file.write_all(&[0])?;
    }
    file.rewind()?;
    Ok(file)
}

const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

pub(super) fn build_network_seccomp() -> Result<BpfProgram, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for network filter: {e}"))?;
    build_network_seccomp_for(arch, libc::SYS_socket, libc::SYS_io_uring_setup)
}

fn build_network_seccomp_for(
    arch: TargetArch,
    socket_syscall: i64,
    io_uring_setup_syscall: i64,
) -> Result<BpfProgram, String> {
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
    rules.insert(socket_syscall, socket_rules);

    // io_uring can create sockets without issuing socket(2), so disabling its setup
    // closes the alternate route whenever network access is denied.
    rules.insert(io_uring_setup_syscall, Vec::new());

    let program = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("building network filter: {e}"))?
    .try_into()
    .map_err(|e| format!("compiling network filter: {e}"))?;
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
            "network filter has {guarded_len} instructions, above the kernel limit of {MAX_BPF_INSTRUCTIONS}"
        ));
    }
    let mut guarded = Vec::with_capacity(guarded_len);
    guarded.extend(guard);
    guarded.append(&mut program);
    Ok(guarded)
}

fn build_probe_seccomp() -> Result<BpfProgram, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|error| format!("unsupported architecture for probe filter: {error}"))?;
    let mut rules = BTreeMap::new();
    rules.insert(libc::SYS_uname, Vec::new());
    SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|error| format!("building probe filter: {error}"))?
    .try_into()
    .map_err(|error| format!("compiling probe filter: {error}"))
}

fn write_seccomp_program(program: BpfProgram) -> std::io::Result<File> {
    let mut file = file_above_stdio(tempfile::tempfile()?)?;
    let byte_len = program.len() * std::mem::size_of::<libc::sock_filter>();
    let bytes = unsafe { std::slice::from_raw_parts(program.as_ptr().cast::<u8>(), byte_len) };
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

fn seal_inherited_fds(command: &mut Command, bubblewrap_data_fds: Vec<i32>) {
    // Keep only stdio and Bubblewrap's harmless setup-data descriptors. This closes
    // the inherited-open-file escape from path denial while retaining Rust's exec
    // error pipe until exec succeeds. Linux 5.11+ marks the whole range atomically;
    // older kernels use a raw procfs descriptor walk that performs no allocation.
    unsafe {
        command.pre_exec(move || {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            let result = libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC);
            if result < 0 {
                cloexec_open_fds_from_proc()?;
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
    fn network_seccomp_rejects_unsupported_x86_abis_before_native_dispatch() {
        const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
        const AUDIT_ARCH_RISCV64: u32 = 0xc000_00f3;
        const X86_64_SOCKET: u32 = 41;
        const X86_64_GETPID: u32 = 39;
        const GENERIC_SOCKET: u32 = 198;
        const GENERIC_GETPID: u32 = 172;
        const IO_URING_SETUP: u32 = 425;

        let denied = u32::from(SeccompAction::Errno(libc::EPERM as u32));
        let allowed = u32::from(SeccompAction::Allow);
        let killed = u32::from(SeccompAction::KillProcess);
        let x86 = build_network_seccomp_for(
            TargetArch::x86_64,
            i64::from(X86_64_SOCKET),
            i64::from(IO_URING_SETUP),
        )
        .unwrap();
        for (syscall, family) in [
            (X86_64_SOCKET, libc::AF_INET),
            (IO_URING_SETUP, 0),
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
                "x86-64 syscall {syscall:#x} escaped the network filter",
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
            let program = build_network_seccomp_for(
                arch,
                i64::from(GENERIC_SOCKET),
                i64::from(IO_URING_SETUP),
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

        let program = build_network_seccomp().unwrap();
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
    fn target_environment_rejects_invalid_keys_and_nuls() {
        let mut setup = Command::new("");
        let mut env = BTreeMap::new();
        env.insert(OsString::from("BAD=KEY"), OsString::from("value"));
        assert!(append_target_environment(&mut setup, &env).is_err());
        env.clear();
        env.insert(
            OsString::from("GOOD"),
            OsString::from_vec(b"bad\0value".to_vec()),
        );
        assert!(append_target_environment(&mut setup, &env).is_err());
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
    fn bwrap_failure_diagnostics_are_host_specific_without_admin_advice() {
        let message = classify_bwrap_failures(&["candidate: unknown option --info-fd".into()]);
        assert!(message.contains("required stock options"), "{message}");
        for banned in ["sudo", "sysctl", "disable AppArmor", "apparmor_parser"] {
            assert!(!message.contains(banned), "{message}");
        }
    }

    #[test]
    fn setsid_discovery_is_absolute_and_missing_helper_fails_without_admin_advice() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("setsid");
        fs::write(&helper, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_setsid_in([helper.clone()]).unwrap(), helper);

        let message = find_setsid_in([temp.path().join("missing")]).unwrap_err();
        assert!(message.contains("setsid launcher"), "{message}");
        for banned in ["sudo", "install", "administrator"] {
            assert!(!message.contains(banned), "{message}");
        }
    }
}
