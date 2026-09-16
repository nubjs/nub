//! Per-OS enforcement backends and the [`apply`] entry that turns a resolved
//! [`SandboxPolicy`] into a launch-ready child.
//!
//! The enforcement contract is FAIL-SAFE-WITH-DEGRADATION, not fail-open (ported
//! from the reviewed salvage `backend/mod.rs`): a backend NEVER silently drops an
//! axis it claimed to enforce. When a primitive is unavailable it records the
//! loss in [`Degradation`] so the caller surfaces a WARNING; a hard fail-closed
//! (a required axis unenforceable) is `Err(Degradation)`.
//!
//! BACKEND STATUS: **Linux only.** [`linux`] (Landlock plus a seccomp `USER_NOTIF`
//! supervisor) is the sole enforcing backend. The macOS Seatbelt and Windows
//! AppContainer backends were removed when the sandbox became Linux-only: neither
//! OS can express the grammar's deny-inside-allow and per-hostname egress at zero
//! privilege, and Seatbelt additionally cannot nest inside another profile at all.
//!
//! This module still COMPILES on every OS so the macOS dev host can type-check it,
//! but off Linux there is no enforcement path — see [`generic_apply`], which reports
//! the sandbox as unsupported rather than silently running the command unconfined.
//!
//! LAUNCH SEAM: every backend returns a [`Prepared`] plan whose command is private.
//! Callers launch through [`Prepared::spawn`], [`Prepared::status`], or
//! [`Prepared::output`], preserving startup verification and resource ownership.

// Off Linux the entire launch path below is unreachable BY CONSTRUCTION: `Sandbox::new`
// refuses before any of it runs. Enumerating that with a `cfg` per helper would put a dozen
// per-OS attributes back into the file this module spent a whole pass taking them out of, and
// it buys nothing — on the one platform where this code RUNS, dead-code detection is intact.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::policy::{Effect, Inspection, ProxyMode, SandboxPolicy};
use crate::proxy::mitm::{BrokerSession, MitmEngine, RuntimeCredentialBroker};
use crate::proxy::{EgressProxy, StaticDecider};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::process::Command;
use std::sync::Arc;

#[cfg(unix)]
mod unix_tmp;
#[cfg(unix)]
use unix_tmp::PrivateTemp;
#[cfg(not(unix))]
type PrivateTemp = tempfile::TempDir;

#[cfg(target_os = "linux")]
mod unix_guardian;

#[cfg(target_os = "linux")]
mod linux_lifetime;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
mod linux_landlock;

#[cfg(target_os = "linux")]
mod linux_supervisor;

/// The Landlock suite's skip gate — `None` when this kernel has no usable Landlock.
/// Test support, not an embedder API.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn landlock_abi() -> Option<u32> {
    linux_landlock::probe_abi()
}

// The macOS and Windows backends were deleted when the sandbox became Linux-only. See
// `A2b` in the effort's TASKS.md: the crate still COMPILES everywhere (so the macOS dev
// host can run `cargo check`), but off Linux `apply` returns `Effect::Unsupported`.

// The OS-agnostic Linux mount-plan derivation. Compiled on Linux (its real consumer)
// and under `test` on any host so authored-order and rejection invariants are tested
// without a Linux kernel.
#[cfg(any(target_os = "linux", test))]
mod linux_grants;

/// Which confinement axes a backend managed to enforce, and which degraded. A
/// non-empty `lost` becomes a user-facing WARNING. Ported contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Degradation {
    /// Axis names that could NOT be enforced (e.g. "fs", "net", "net-per-host").
    pub lost: Vec<String>,
    /// A one-line reason (missing primitive, unsupported OS), surfaced with the
    /// lost-axis list.
    pub reason: Option<String>,
}

impl Degradation {
    /// Full enforcement — nothing lost.
    pub fn full() -> Self {
        Self::default()
    }
    pub fn is_full(&self) -> bool {
        self.lost.is_empty()
    }
    /// The one-line WARNING text, or `None` when fully enforced.
    pub fn warning(&self) -> Option<String> {
        if self.lost.is_empty() {
            return None;
        }
        let axes = self.lost.join(", ");
        Some(match &self.reason {
            Some(r) => format!("sandbox running in reduced mode — {axes} not enforced ({r})"),
            None => format!("sandbox running in reduced mode — {axes} not enforced"),
        })
    }
}

/// The command to launch under a policy. Host-provided (Boundary B).
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: std::ffi::OsString,
    pub args: Vec<std::ffi::OsString>,
    /// Working directory for the child, if the caller pins one.
    pub cwd: Option<std::path::PathBuf>,
    /// Directories whose existing immediate children may be materialized for
    /// bounded deny globs such as `.env*` and `*.sandbox.json`. The frontend adds
    /// the workspace root and each package root; no backend recursively walks them.
    pub deny_search_roots: Vec<std::path::PathBuf>,
    /// Pipe the child's stdout so the host can stream it through a redactor before
    /// forwarding. ONLY the request crosses this boundary — never the secret values
    /// (the host holds those and does the scrub). Default `false` = inherit (today's
    /// behavior, byte-for-byte).
    pub redact_stdout: bool,
    /// Pipe the child's stderr for host-side redaction. See [`redact_stdout`](Self::redact_stdout).
    pub redact_stderr: bool,
    /// Legacy backend grouping hint, retained for existing embedders. Reusable
    /// launches always own their commands: Unix uses a private guardian process
    /// group, and Windows uses a kill-on-close Job assigned during process creation.
    /// This flag cannot request detached commands. Hosts should forward signals
    /// through [`Prepared::spawn_with_signal_target`] rather than assume that the
    /// command remains in the host's terminal process group.
    pub reap_descendants: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            deny_search_roots: Vec::new(),
            redact_stdout: false,
            redact_stderr: false,
            reap_descendants: false,
        }
    }
    pub fn arg(mut self, a: impl Into<std::ffi::OsString>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn cwd(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
    pub fn deny_search_root(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.deny_search_roots.push(dir.into());
        self
    }
    pub fn deny_search_roots<I, P>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<std::path::PathBuf>,
    {
        self.deny_search_roots
            .extend(dirs.into_iter().map(Into::into));
        self
    }
    pub fn redact_stdout(mut self, redact: bool) -> Self {
        self.redact_stdout = redact;
        self
    }
    pub fn redact_stderr(mut self, redact: bool) -> Self {
        self.redact_stderr = redact;
        self
    }
    pub fn reap_descendants(mut self, reap: bool) -> Self {
        self.reap_descendants = reap;
        self
    }
}

/// A launch-ready child. The command stays private so backend supervision and
/// cleanup cannot be bypassed; callers launch through [`Prepared::spawn`],
/// [`Prepared::status`], or [`Prepared::output`].
pub struct Prepared {
    /// The configured child for the mac/linux/skeleton path. On Windows this is the
    /// env-scrubbed plain child used ONLY when no confinement mechanism applies
    /// (`launch` is `None`); when one does — either Windows variant — `launch` owns the
    /// spawn and this field is unused.
    command: Command,
    pub degradation: Degradation,
    /// The running egress proxy (design.md §2.5), when the policy enforces per-host
    /// net. It runs in the nub PARENT and MUST outlive the child, so it is owned here:
    /// [`Prepared::status`] holds it for the child's whole run, and dropping this
    /// value stops the listener. `None` when net is unconfined or coarse-deny (no
    /// proxy needed). Set by [`apply`], not the per-OS backends.
    pub(crate) proxy: Option<EgressProxy>,
    /// Shared session state for reusable launches. Unlike the compatibility fields above,
    /// this is reference counted: closing a [`Sandbox`] does not tear down resources while
    /// one of its submitted commands is still running.
    pub(crate) session: Option<Arc<SessionResources>>,
    /// Files whose descriptors the Landlock backend consumes after fork (the ruleset fd its
    /// `pre_exec` hook restricts against). Keeping them here guarantees they remain open until
    /// `command` is spawned.
    #[cfg(target_os = "linux")]
    pub(crate) _inherited_files: Vec<std::fs::File>,
    /// Legacy backend request for an additional membership check. Supported Unix
    /// launches join an owned guardian regardless; other Unix targets only signal
    /// a process group after the kernel confirms the backend's requested grouping.
    #[cfg(unix)]
    pub(crate) signal_process_group: bool,
    /// Compatibility owner for a one-shot private tmp directory. Reusable sessions retain
    /// their stable managed tmp root in [`SessionResources`] instead.
    pub(crate) _private_tmp: Option<PrivateTemp>,
    /// Pipe stdout/stderr at spawn so the host can drain them through an output redactor.
    /// Copied from [`CommandSpec`] in [`apply`], applied in
    /// [`Prepared::spawn_with_signal_target`]. Both `false` (the default) = inherit,
    /// byte-for-byte today's behavior.
    pub(crate) redact_stdout: bool,
    pub(crate) redact_stderr: bool,
    /// Linux supervised-launch plan (seccomp `USER_NOTIF` per-host egress). When `Some`,
    /// [`status`](Self::status) forks the confined child via
    /// [`linux_supervisor::spawn_supervised`] instead of spawning `command` — the listener-fd
    /// barrier cannot ride `Command::spawn` (see [`linux_supervisor::SupervisedChild`]).
    /// Mirrors the Windows `launch` field. The asynchronous [`spawn`](Self::spawn) path does
    /// not yet host it and refuses rather than launch unsupervised.
    #[cfg(target_os = "linux")]
    pub(crate) supervised: Option<SupervisedPlan>,
}

/// The owned inputs a supervised Linux launch forks with, built by [`apply`] and held until
/// [`Prepared::status`] runs it. Owns its data because [`linux_supervisor::spawn_supervised`]
/// borrows the argv/envp/cwd across the fork; the ruleset (when present) is held open so its
/// descriptor survives to the child's `restrict_self`.
#[cfg(target_os = "linux")]
pub(crate) struct SupervisedPlan {
    pub(crate) egress: linux_supervisor::EgressPolicy,
    pub(crate) argv: Vec<CString>,
    pub(crate) envp: Vec<CString>,
    pub(crate) cwd: Option<CString>,
    /// Landlock ruleset held open until the fork consumes its fd; `None` = no fs boundary.
    pub(crate) ruleset: Option<linux_landlock::LandlockRuleset>,
    pub(crate) seccomp_ceiling: Option<Vec<seccompiler::sock_filter>>,
    pub(crate) ca_bundle: Option<std::fs::File>,
}

#[cfg(target_os = "linux")]
impl SupervisedPlan {
    /// Fork the confined child and return its owned supervisor/stdio handle.
    fn spawn(
        self,
        stdin: linux_supervisor::SupervisedStdio,
        stdout: linux_supervisor::SupervisedStdio,
        stderr: linux_supervisor::SupervisedStdio,
    ) -> std::io::Result<linux_supervisor::SupervisedChild> {
        self.spawn_with_ready(stdin, stdout, stderr, |_| Ok(()))
    }

    fn spawn_with_ready(
        self,
        stdin: linux_supervisor::SupervisedStdio,
        stdout: linux_supervisor::SupervisedStdio,
        stderr: linux_supervisor::SupervisedStdio,
        ready: impl FnOnce(i32) -> std::io::Result<()>,
    ) -> std::io::Result<linux_supervisor::SupervisedChild> {
        let SupervisedPlan {
            egress,
            argv,
            envp,
            cwd,
            ruleset,
            seccomp_ceiling,
            ca_bundle,
        } = self;
        let inherited_fds: Vec<_> = ca_bundle
            .iter()
            .map(std::os::fd::AsRawFd::as_raw_fd)
            .collect();
        let launch = linux_supervisor::SupervisedLaunch {
            argv: &argv,
            envp: &envp,
            cwd: cwd.as_deref(),
            ruleset_fd: ruleset
                .as_ref()
                .map_or(-1, linux_landlock::LandlockRuleset::as_raw_fd),
            seccomp_ceiling: seccomp_ceiling.as_deref(),
            stdin,
            stdout,
            stderr,
            inherited_fds: &inherited_fds,
        };
        let child = linux_supervisor::spawn_supervised_with_ready(egress, launch, ready);
        // Keep the ruleset alive across the fork+exec, exactly as the `Command` path keeps
        // `_inherited_files`: the child's `restrict_self` consumes the fd after fork.
        drop(ruleset);
        drop(ca_bundle);
        child
    }

    /// Compatibility adapter for synchronous callers.
    fn run(self) -> std::io::Result<std::process::ExitStatus> {
        self.spawn(
            linux_supervisor::SupervisedStdio::Inherit,
            linux_supervisor::SupervisedStdio::Inherit,
            linux_supervisor::SupervisedStdio::Inherit,
        )?
        .wait()
    }
}

/// A running prepared child together with every resource that must outlive it.
/// Dropping the handle kills and reaps the child before releasing those resources.
pub struct PreparedChild {
    child: Option<std::process::Child>,
    #[cfg(target_os = "linux")]
    supervised_child: Option<linux_supervisor::SupervisedChild>,
    child_id: u32,
    #[cfg(target_os = "linux")]
    guardian: Option<unix_guardian::UnixGuardian>,
    #[cfg(unix)]
    signal_target: Option<i32>,
    _proxy: Option<EgressProxy>,
    _private_tmp: Option<PrivateTemp>,
    _session: Option<Arc<SessionResources>>,
}

/// A reusable, resolved sandbox lifecycle.
///
/// Acquisition snapshots any credential-broker environment values, starts the required
/// egress proxy, and allocates managed private storage once. [`prepare`](Self::prepare)
/// never reads ambient environment state; it only turns a command description into a
/// [`Prepared`] launch. Cloning a `Sandbox` creates another lease to the same immutable
/// policy and resources. Dropping every lease closes those resources after all submitted
/// [`PreparedChild`] values have ended.
#[derive(Clone)]
pub struct Sandbox {
    resources: Arc<SessionResources>,
}

/// Session-owned state deliberately kept private: callers can submit commands, not mutate
/// policy, credentials, proxy identity, or the managed temporary root after acquisition.
pub(crate) struct SessionResources {
    policy: SandboxPolicy,
    proxy: Option<SessionProxy>,
    private_tmp: Option<PrivateTemp>,
    #[cfg(target_os = "linux")]
    retained_grants: linux::RetainedLinuxGrants,
}

type SessionProxy = EgressProxy;

impl Sandbox {
    /// Acquire a reusable sandbox from an already-resolved policy.
    ///
    /// This is the sole compatibility ambient lookup: credential values are captured here
    /// for the broker session and are never re-read for later command submissions.
    #[cfg(target_os = "linux")]
    pub fn new(policy: &SandboxPolicy) -> Result<Self, Degradation> {
        if !policy.env.resolved {
            return Err(Degradation {
                lost: vec!["env-unresolved".to_string()],
                reason: Some(
                    "sandbox policy has no resolved target environment; compile it with an ambient snapshot before acquisition"
                        .to_string(),
                ),
            });
        }

        let mut runtime_policy = policy.clone();
        initialize_shared_tool_state(&runtime_policy)?;
        let runtime_brokers = capture_runtime_brokers(policy, &mut runtime_policy)?;
        let proxy = start_session_proxy(&runtime_policy, runtime_brokers)?;
        let private_tmp = make_private_tmp(&runtime_policy)?;
        #[cfg(target_os = "linux")]
        let retained_grants = linux::capture_retained_grants(&runtime_policy)?;
        Ok(Self {
            resources: Arc::new(SessionResources {
                policy: runtime_policy,
                proxy,
                private_tmp,
                #[cfg(target_os = "linux")]
                retained_grants,
            }),
        })
    }

    /// Off Linux there is nothing to acquire. Refusing HERE rather than at launch keeps a
    /// sandbox that can never run from starting a proxy, minting a bearer token and making a
    /// private tmp root on the way to the same answer.
    #[cfg(not(target_os = "linux"))]
    pub fn new(_policy: &SandboxPolicy) -> Result<Self, Degradation> {
        Err(unsupported_platform())
    }

    /// Alias for [`Sandbox::new`], spelling the lifecycle operation used by embedders that
    /// maintain a registry/cache of reusable sandboxes.
    pub fn acquire(policy: &SandboxPolicy) -> Result<Self, Degradation> {
        Self::new(policy)
    }

    /// Prepare one command under this session's immutable policy and retained resources.
    pub fn prepare(&self, spec: CommandSpec) -> Result<Prepared, Degradation> {
        prepare_with_resources(&self.resources, spec)
    }

    /// Release this caller's session lease. Submitted commands retain their own lease until
    /// they exit, so closing a sandbox never tears down another active command's resources.
    pub fn close(self) {}
}

fn initialize_shared_tool_state(policy: &SandboxPolicy) -> Result<(), Degradation> {
    for rule in &policy.fs.rules.entries {
        if rule.origin != crate::policy::FsOrigin::SharedToolState
            || rule.effect != crate::policy::Effect::Allow
            || rule.access != crate::policy::FsAccess::ReadWrite
            || rule.matcher.as_str().contains('*')
        {
            continue;
        }
        std::fs::create_dir_all(rule.matcher.as_str()).map_err(|error| Degradation {
            lost: vec!["tool-state".into()],
            reason: Some(format!(
                "cannot initialize shared tool directory {}: {error}",
                rule.matcher.as_str()
            )),
        })?;
    }
    Ok(())
}

/// Remove idle sandbox scratch directories, recovering interrupted cleanup first.
/// Failures are returned and their ownership records remain available for retry.
pub fn cleanup() -> std::io::Result<()> {
    #[cfg(unix)]
    return unix_tmp::cleanup();
    #[cfg(not(unix))]
    Ok(())
}

/// The signal destination authenticated during [`Prepared::spawn_with_signal_target`].
///
/// The retained-monitor `Callback` variant was dropped with `linux_monitor` (epic 1.1); the
/// Landlock path — the only supervised Linux launch today — reaps its child's process group by a
/// `Direct` negative pgid, so this carries a plain signal target.
#[doc(hidden)]
pub enum PreparedSignalTarget {
    Direct(i32),
}

impl PreparedChild {
    pub fn id(&self) -> u32 {
        self.child_id
    }

    /// The command's owned process-group id. On Linux/macOS the guardian, not
    /// necessarily the direct child, leads this group. Windows uses a Job instead.
    ///
    /// Exposed so a host can register the group with its own terminate-signal reaper: a
    /// signal whose default action kills nub never runs this handle's `Drop`, so the
    /// group would otherwise survive nub itself. `None` on every other path — the child
    /// shares nub's group there and signalling `-pgid` would kill nub and every sibling.
    #[cfg(unix)]
    pub fn process_group_id(&self) -> Option<i32> {
        self.signal_target
            .filter(|target| *target < 0)
            .map(|target| -target)
    }

    /// Take the piped stdout handle (present only when the launch requested
    /// `redact_stdout`). The host drains it through its output redactor. `None` when
    /// stdout was inherited or already taken.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        #[cfg(target_os = "linux")]
        if let Some(child) = self.supervised_child.as_mut() {
            return child.take_stdout();
        }
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// Take the piped stderr handle. See [`take_stdout`](Self::take_stdout).
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        #[cfg(target_os = "linux")]
        if let Some(child) = self.supervised_child.as_mut() {
            return child.take_stderr();
        }
        self.child.as_mut().and_then(|c| c.stderr.take())
    }

    /// Take the piped stdin handle, if this command was launched with piped input.
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        #[cfg(target_os = "linux")]
        if let Some(child) = self.supervised_child.as_mut() {
            return child.take_stdin();
        }
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(target_os = "linux")]
        if let Some(mut child) = self.supervised_child.take() {
            let result = child.wait();
            self.release_resources();
            return result;
        }
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| prepared_child_reaped_error("wait"))?;
        let result = wait_child_eintr(child);
        if result.is_ok() {
            // The jailed script may have forked a build daemon that outlived it. Signalling the
            // child's own process group is the Landlock path's only equivalent of the removed
            // bubblewrap PID namespace's implicit reap. Best-effort: an empty group is ESRCH,
            // which is the normal case and not an error.
            //
            // Reaping on the SUCCESSFUL return too is the point, not a side effect: the measured
            // leak is a script that exits 0 having backgrounded a writer, whose output then keeps
            // landing in a package dir the installer already snapshotted.
            #[cfg(unix)]
            if let Some(target) = self.signal_target.filter(|target| *target < 0) {
                unsafe { libc::kill(target, libc::SIGKILL) };
            }
            self.child.take();
            self.release_resources();
        }
        result
    }

    /// Wait for completion, or drop the confined process tree when its owner cancels.
    pub fn wait_cancellable(
        &mut self,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<std::process::ExitStatus> {
        loop {
            #[cfg(target_os = "linux")]
            if let Some(child) = self.supervised_child.as_mut() {
                if let Some(status) = child.try_wait()? {
                    self.supervised_child.take();
                    self.release_resources();
                    return Ok(status);
                }
                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    let mut child = self.supervised_child.take().expect("checked above");
                    let _ = child.kill();
                    let _ = child.wait();
                    self.release_resources();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "sandbox launch cancelled",
                    ));
                }
                child.wait_for_exit_event()?;
                continue;
            }
            let child = self
                .child
                .as_mut()
                .ok_or_else(|| prepared_child_reaped_error("wait_cancellable"))?;
            if try_wait_child_eintr(child)?.is_some() {
                return self.wait();
            }
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                if let Some(mut child) = self.child.take() {
                    #[cfg(unix)]
                    kill_and_reap(&mut child, self.signal_target);
                    #[cfg(not(unix))]
                    kill_and_reap(&mut child);
                }
                self.release_resources();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "sandbox launch cancelled",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    pub fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        use std::io::Read;

        let stdout = self.take_stdout();
        let stderr = self.take_stderr();
        let stdout = std::thread::Builder::new()
            .name("nub-sandbox-stdout".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = stdout {
                    pipe.read_to_end(&mut bytes)?;
                }
                Ok::<_, std::io::Error>(bytes)
            })?;
        let stderr = std::thread::Builder::new()
            .name("nub-sandbox-stderr".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = stderr {
                    pipe.read_to_end(&mut bytes)?;
                }
                Ok::<_, std::io::Error>(bytes)
            })?;
        let status = self.wait();
        let stdout = stdout
            .join()
            .map_err(|_| std::io::Error::other("sandbox stdout drain thread panicked"))??;
        let stderr = stderr
            .join()
            .map_err(|_| std::io::Error::other("sandbox stderr drain thread panicked"))??;
        Ok(std::process::Output {
            status: status?,
            stdout,
            stderr,
        })
    }

    fn release_resources(&mut self) {
        #[cfg(target_os = "linux")]
        self.guardian.take();
        // Drop order matters: the proxy before the private tmp dir it may have written into.
        self._proxy.take();
        self._private_tmp.take();
        self._session.take();
    }
}

fn prepared_child_reaped_error(operation: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("cannot {operation} a prepared child that has already been reaped"),
    )
}

impl Drop for PreparedChild {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if self.supervised_child.take().is_some() {
            self.release_resources();
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        kill_and_reap(&mut child, self.signal_target);
        #[cfg(not(unix))]
        kill_and_reap(&mut child);
    }
}

#[cfg(unix)]
fn kill_and_reap(child: &mut std::process::Child, signal_target: Option<i32>) {
    // The leader can exit between a cancellation poll and teardown while its
    // descendants remain. Reap the confirmed group even if try_wait reaps the leader.
    if let Some(group) = signal_target.filter(|target| *target < 0) {
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    }
    if try_wait_child_eintr(child).ok().flatten().is_some() {
        return;
    }
    if let Some(signal_target) = signal_target.filter(|target| *target != 0) {
        unsafe {
            libc::kill(signal_target, libc::SIGKILL);
        }
        // Let a supervising launcher reap its own target and exit. Killing the
        // launcher immediately can orphan the target as a host-visible zombie.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if try_wait_child_eintr(child).ok().flatten().is_some() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let _ = child.kill();
    let _ = wait_child_eintr(child);
}

#[cfg(not(unix))]
fn kill_and_reap(child: &mut std::process::Child) {
    if try_wait_child_eintr(child).ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = wait_child_eintr(child);
}

/// Confirm membership in the launch-owned group without changing it. Never signal
/// the host's group. A failed pre-exec join fails spawn; ESRCH is accepted for an
/// already-exited, still-unreaped child (macOS getpgid rejects zombies). The caller
/// retains the guardian, so its expected group identity cannot be recycled here.
#[cfg(unix)]
fn confirm_group_membership(pid: i32, expected: i32) -> bool {
    // SAFETY: `getpgid` on a child of this process, `getpgrp` on ourselves — plain reads.
    unsafe {
        if expected == libc::getpgrp() || expected <= 0 {
            return false;
        }
        match libc::getpgid(pid) {
            -1 => std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            pgid => pgid == expected,
        }
    }
}

fn wait_child_eintr(child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn try_wait_child_eintr(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    loop {
        match child.try_wait() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

impl Prepared {
    /// Spawn the child without exposing the backend command. The returned handle
    /// owns every launch resource and kills/reaps on an early drop.
    pub fn spawn(self) -> std::io::Result<PreparedChild> {
        self.spawn_with_signal_target(|_| Ok(()))
    }

    /// Install a signal target while a supervised Linux child is still blocked.
    #[doc(hidden)]
    pub fn spawn_with_signal_target(
        mut self,
        ready: impl FnOnce(PreparedSignalTarget) -> std::io::Result<()>,
    ) -> std::io::Result<PreparedChild> {
        {
            #[cfg(target_os = "linux")]
            if let Some(plan) = self.supervised.take() {
                let stdout = if self.redact_stdout {
                    linux_supervisor::SupervisedStdio::Piped
                } else {
                    linux_supervisor::SupervisedStdio::Inherit
                };
                let stderr = if self.redact_stderr {
                    linux_supervisor::SupervisedStdio::Piped
                } else {
                    linux_supervisor::SupervisedStdio::Inherit
                };
                let child = plan.spawn_with_ready(
                    linux_supervisor::SupervisedStdio::Inherit,
                    stdout,
                    stderr,
                    |group| ready(PreparedSignalTarget::Direct(-group)),
                )?;
                let child_id = child.id();
                let signal_target = child.process_group_id().map(|group| -group);
                return Ok(PreparedChild {
                    child: None,
                    supervised_child: Some(child),
                    guardian: None,
                    child_id,
                    signal_target,
                    _proxy: self.proxy.take(),
                    _private_tmp: self._private_tmp.take(),
                    _session: self.session.take(),
                });
            }
            // Pipe the requested fds so the host can drain them through its redactor. stdin is
            // left untouched (interactive input still reaches the child). Both flags off (the
            // default) = inherit, so the non-redacting path is byte-for-byte unchanged.
            if self.redact_stdout {
                self.command.stdout(std::process::Stdio::piped());
            }
            if self.redact_stderr {
                self.command.stderr(std::process::Stdio::piped());
            }
            #[cfg(target_os = "linux")]
            let guardian = {
                let guardian = unix_guardian::UnixGuardian::start()?;
                guardian.join_command(&mut self.command);
                #[cfg(target_os = "linux")]
                linux_lifetime::attach(&mut self.command)?;
                guardian
            };
            #[allow(unused_mut)]
            let mut child = self.command.spawn()?;
            // The Landlock backend inherits its ruleset descriptor across spawn; the parent copy
            // can close the moment the child holds it (the `pre_exec` hook consumes it after fork).
            #[cfg(target_os = "linux")]
            self._inherited_files.clear();
            // A failed guardian pre-exec hook fails spawn. Retain the backend's
            // requested membership cross-check before handing a negative target out.
            #[cfg(target_os = "linux")]
            if self.signal_process_group
                && !confirm_group_membership(child.id() as i32, guardian.process_group_id())
            {
                kill_and_reap(&mut child, Some(-guardian.process_group_id()));
                return Err(std::io::Error::other(
                    "sandbox command did not join its owner-death guardian",
                ));
            }
            #[cfg(all(unix, not(target_os = "linux")))]
            let signal_process_group = self.signal_process_group
                && confirm_group_membership(child.id() as i32, child.id() as i32);
            // Negative targets name the private guardian group, never the host group.
            #[cfg(unix)]
            let signal_target = {
                #[cfg(target_os = "linux")]
                let target = -guardian.process_group_id();
                #[cfg(not(target_os = "linux"))]
                let target = if signal_process_group {
                    -(child.id() as i32)
                } else {
                    child.id() as i32
                };
                if let Err(error) = ready(PreparedSignalTarget::Direct(target)) {
                    kill_and_reap(&mut child, Some(target));
                    return Err(error);
                }
                Some(target)
            };
            #[cfg(not(unix))]
            let _ = ready;
            let child_id = child.id();
            Ok(PreparedChild {
                child: Some(child),
                #[cfg(target_os = "linux")]
                supervised_child: None,
                child_id,
                #[cfg(target_os = "linux")]
                guardian: Some(guardian),
                #[cfg(unix)]
                signal_target,
                _proxy: self.proxy.take(),
                _private_tmp: self._private_tmp.take(),
                _session: self.session.take(),
            })
        }
    }

    /// Launch and wait, retaining the backend's process-tree and resource ownership.
    #[allow(unused_mut)]
    pub fn status(mut self) -> std::io::Result<std::process::ExitStatus> {
        // The Linux supervised launch owns its own fork+wait (the connect-notifier supervisor
        // runs in this process for the child's whole life).
        #[cfg(target_os = "linux")]
        if let Some(plan) = self.supervised.take() {
            return plan.run();
        }
        let mut child = self.spawn()?;
        child.wait()
    }

    /// Launch with cancellation, preserving the backend's process-tree cleanup.
    #[allow(unused_mut)]
    pub fn status_cancellable(
        mut self,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<std::process::ExitStatus> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "sandbox launch cancelled",
            ));
        }
        self.spawn()?.wait_cancellable(cancelled)
    }

    /// Launch, wait, and capture stdout/stderr through the supervised seam.
    pub fn output(mut self) -> std::io::Result<std::process::Output> {
        #[cfg(target_os = "linux")]
        if let Some(plan) = self.supervised.take() {
            let child = plan.spawn(
                linux_supervisor::SupervisedStdio::Null,
                linux_supervisor::SupervisedStdio::Piped,
                linux_supervisor::SupervisedStdio::Piped,
            )?;
            let child_id = child.id();
            let signal_target = child.process_group_id().map(|group| -group);
            return PreparedChild {
                child: None,
                supervised_child: Some(child),
                guardian: None,
                child_id,
                signal_target,
                _proxy: self.proxy.take(),
                _private_tmp: self._private_tmp.take(),
                _session: self.session.take(),
            }
            .wait_with_output();
        }
        self.command.stdin(std::process::Stdio::null());
        self.redact_stdout = true;
        self.redact_stderr = true;
        self.spawn()?.wait_with_output()
    }
}

/// Whether the policy needs the egress proxy. Coarse `net: true` / `net: false`
/// never start one. An ordinary per-host allow needs it — the compiler auto-derives
/// the proxy tier from the policy — and a credential broker additionally requires a
/// terminating (TLS-inspecting) proxy. Startup failure is a hard fail-closed apply
/// error: a required proxy is never represented as "not needed."
fn proxy_needed(policy: &SandboxPolicy) -> bool {
    policy.net.enforce
        && (!policy.net.brokers.is_empty()
            || (policy
                .net
                .rules
                .iter()
                .any(|rule| rule.effect == Effect::Allow)
                && policy.net.mode != ProxyMode::Disabled))
}

/// Capture credential material exactly once, while acquiring a session. The resulting broker
/// owns values and marker substitutions; later command preparation uses only the immutable
/// `SandboxPolicy` stored in [`SessionResources`].
fn capture_runtime_brokers(
    policy: &SandboxPolicy,
    runtime_policy: &mut SandboxPolicy,
) -> Result<Vec<RuntimeCredentialBroker>, Degradation> {
    if policy.net.brokers.is_empty() {
        return Ok(Vec::new());
    }
    let session = BrokerSession::from_policy(&policy.net.brokers, |name| {
        let Some(value) = std::env::var_os(name) else {
            return Ok(None);
        };
        value
            .into_string()
            .map(Some)
            .map_err(|_| "value is not valid Unicode".to_string())
    })
    .map_err(|error| Degradation {
        lost: vec!["credential-broker".to_string()],
        reason: Some(error.to_string()),
    })?;
    session.install_markers(&mut runtime_policy.env.constructed);
    Ok(session.into_brokers())
}

/// Acquire parent-owned proxy state once, shared by every command in the session.
fn start_session_proxy(
    policy: &SandboxPolicy,
    runtime_brokers: Vec<RuntimeCredentialBroker>,
) -> Result<Option<SessionProxy>, Degradation> {
    proxy_context(policy, runtime_brokers)?
        .map(|context| {
            context.start().map_err(|error| Degradation {
                lost: vec!["net-per-host".to_string()],
                reason: Some(format!("starting required egress proxy: {error}")),
            })
        })
        .transpose()
}

fn proxy_context(
    policy: &SandboxPolicy,
    runtime_brokers: Vec<RuntimeCredentialBroker>,
) -> Result<Option<crate::proxy::ProxyContext>, Degradation> {
    if !proxy_needed(policy) {
        return Ok(None);
    }
    let decider = Arc::new(StaticDecider::new(policy.net.clone()));
    let mitm = match policy.net.inspection {
        Inspection::TlsInspect => {
            let terminate_all = matches!(policy.net.mode, ProxyMode::Terminate);
            match MitmEngine::new(runtime_brokers, terminate_all) {
                Ok(engine) => Some(engine),
                Err(error) => {
                    return Err(Degradation {
                        lost: vec!["net-per-host".to_string()],
                        reason: Some(format!("starting required TLS-inspection proxy: {error}")),
                    });
                }
            }
        }
        Inspection::Connection => None,
    };
    Ok(Some(crate::proxy::ProxyContext { decider, mitm }))
}

/// Child-only trust settings. Replacement stores need the real roots alongside
/// the ephemeral CA; Node's additional store remains additive.
pub(super) const CA_ENV_KEYS: &[&str] = &[
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "PIP_CERT",
    "NPM_CONFIG_CAFILE",
    "npm_config_cafile",
    "CARGO_HTTP_CAINFO",
    "AWS_CA_BUNDLE",
    "DENO_CERT",
];

#[cfg_attr(target_os = "linux", allow(dead_code))]
fn set_ca_env(command: &mut Command, bundle: &std::path::Path) {
    for key in CA_ENV_KEYS {
        command.env(key, bundle);
    }
}

/// One-line stderr notice when TLS termination engages — the honesty bar (§5, option 2):
/// nub never silently decrypts, even when the user's own config demanded it.
fn emit_mitm_notice(policy: &SandboxPolicy) {
    let scope = if policy.net.brokers.is_empty() {
        "all allowed hosts".to_string()
    } else {
        policy
            .net
            .brokers
            .iter()
            .map(|b| b.host.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!(
        "sandbox: TLS termination engaged for {scope} — request inspection runs in-proxy \
         (ephemeral per-run CA, child-scoped via NODE_EXTRA_CA_CERTS-class env, never added \
         to the OS trust store)"
    );
}

/// Apply a resolved policy to the unprivileged backend for this operating system.
/// Environment filtering constructs the child's environment. Unsupported required
/// guarantees fail closed; best-effort losses remain visible on `Prepared`.
pub fn apply(policy: &SandboxPolicy, spec: CommandSpec) -> Result<Prepared, Degradation> {
    Sandbox::new(policy)?.prepare(spec)
}

/// Compatibility implementation behind [`Sandbox::prepare`]. The policy and every resource
/// it refers to were frozen at acquisition, so this function must not consult ambient state.
#[cfg(target_os = "linux")]
fn prepare_with_resources(
    resources: &Arc<SessionResources>,
    spec: CommandSpec,
) -> Result<Prepared, Degradation> {
    let policy = &resources.policy;
    validate_apply_inputs(policy, &spec)?;
    // Captured before the per-OS backend consumes `spec`; re-applied to the returned
    // `Prepared` below (ONE place) so every backend inherits the stdio-redaction request
    // without threading the boolean through each `apply`.
    let redact_stdout = spec.redact_stdout;
    let redact_stderr = spec.redact_stderr;
    #[cfg(target_os = "linux")]
    let linux_preflight = linux::preflight(policy, &spec)?;
    let proxy_port = resources.proxy.as_ref().map(EgressProxy::port);
    let proxy_token = resources.proxy.as_ref().map(EgressProxy::token);
    let ca_bundle = resources
        .proxy
        .as_ref()
        .map(EgressProxy::ca_bundle_file)
        .transpose()
        .map_err(|error| Degradation {
            lost: vec!["credential-broker".into()],
            reason: Some(format!("duplicating child CA bundle: {error}")),
        })?
        .flatten();
    let ca_bundle_present = ca_bundle.is_some();

    // The acquired session owns the stable managed PRIVATE tmp root (when the policy asks).
    // Its path is threaded into each backend before its command profile is built; all commands
    // in this explicit session intentionally share that private state. `None` for Shared/Deny.
    let tmp_dir = resources.private_tmp.as_ref().map(|d| d.path());

    // The Landlock arm ignores the proxy pair (coarse seccomp family ceiling, no netns); the
    // supervised arm redirects an allowed connect through the loopback proxy (epic 5.1).
    let mut prepared = linux::apply(
        policy,
        spec,
        tmp_dir,
        &resources.retained_grants,
        linux_preflight,
        linux::ProxyAttachment {
            port: proxy_port,
            token: proxy_token,
            ca_bundle,
        },
    )?;

    // Announce TLS inspection only when preparation retained its network enforcement.
    if ca_bundle_present
        && !prepared
            .degradation
            .lost
            .iter()
            .any(|l| l.starts_with("net-per"))
    {
        emit_mitm_notice(policy);
    }

    prepared.session = Some(resources.clone());
    prepared.redact_stdout = redact_stdout;
    prepared.redact_stderr = redact_stderr;
    Ok(prepared)
}

/// The non-Linux half of [`prepare_with_resources`]: there isn't one.
///
/// The crate still COMPILES on macOS and Windows — the dev host is macOS, and a crate that
/// cannot be `cargo check`ed there forces every typo fix onto a remote box — but no other
/// platform has a mechanism that enforces this policy at zero privilege. The honest answer is
/// to refuse the launch. The alternative it replaces was worse than nothing: a skeleton that
/// ran the command UNCONFINED and reported the missing axes as `Degradation`, which reads as
/// a sandbox to anyone who does not check the losses.
#[cfg(not(target_os = "linux"))]
fn prepare_with_resources(
    resources: &Arc<SessionResources>,
    spec: CommandSpec,
) -> Result<Prepared, Degradation> {
    validate_apply_inputs(&resources.policy, &spec)?;
    Err(unsupported_platform())
}

/// The refusal every non-Linux entry point returns. One place, so the wording cannot drift
/// between the acquisition gate and the launch gate.
#[cfg(not(target_os = "linux"))]
fn unsupported_platform() -> Degradation {
    Degradation {
        lost: vec!["fs".into(), "net".into(), "vars".into(), "secrets".into()],
        reason: Some(format!(
            "the sandbox runs on Linux only; this is {}",
            std::env::consts::OS
        )),
    }
}

fn validate_apply_inputs(policy: &SandboxPolicy, spec: &CommandSpec) -> Result<(), Degradation> {
    let has_allow = policy
        .net
        .rules
        .iter()
        .any(|rule| rule.effect == Effect::Allow);
    if policy.net.enforce
        && has_allow
        && policy.net.brokers.is_empty()
        && policy.net.mode == ProxyMode::Disabled
    {
        return Err(Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(
                "fine-grained net allow did not derive an egress proxy tier (internal invariant)"
                    .to_string(),
            ),
        });
    }
    if !policy.net.brokers.is_empty()
        && (!policy.net.enforce
            || policy.net.inspection != Inspection::TlsInspect
            || policy.net.mode == ProxyMode::Passthrough)
    {
        return Err(Degradation {
            lost: vec!["credential-broker".to_string()],
            reason: Some(
                "credential broker IR does not require an enforceable TLS-inspection tier"
                    .to_string(),
            ),
        });
    }
    for (broker_index, broker) in policy.net.brokers.iter().enumerate() {
        let host = crate::matcher::host::strip_trailing_dot(&broker.host);
        let duplicate_host = policy.net.brokers[..broker_index].iter().any(|existing| {
            crate::matcher::host::strip_trailing_dot(&existing.host).eq_ignore_ascii_case(host)
        });
        let valid_host = !broker.host.contains(['*', '/'])
            && !broker.host.starts_with('<')
            && host.parse::<std::net::IpAddr>().is_err()
            && !crate::policy::broker_host_is_legacy_ipv4_literal(host)
            && !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid_host
            || duplicate_host
            || broker.env.is_empty()
            || !crate::matcher::HostMatcher::new(&policy.net).admits(host)
        {
            return Err(Degradation {
                lost: vec!["credential-broker".to_string()],
                reason: Some(format!(
                    "invalid credential broker IR for exact host {:?}",
                    broker.host
                )),
            });
        }
        for (index, name) in broker.env.iter().enumerate() {
            let invalid_name = name.is_empty()
                || name.contains(['=', '\0', '*', '?', '[', ']', '{', '}'])
                || name.ends_with('?')
                || crate::policy::credential_env_name_is_reserved(name);
            let duplicate = broker.env[..index].iter().any(|existing| {
                if cfg!(windows) {
                    existing.eq_ignore_ascii_case(name)
                } else {
                    existing == name
                }
            });
            if invalid_name || duplicate {
                return Err(Degradation {
                    lost: vec!["credential-broker".to_string()],
                    reason: Some(format!(
                        "invalid credential broker environment name at {}[{}]",
                        broker.host, index
                    )),
                });
            }
        }
    }
    let reject_nul = |label: &str, value: &std::ffi::OsStr| {
        if os_str_contains_nul(value) {
            Err(Degradation {
                lost: vec!["process-input".to_string()],
                reason: Some(format!("sandbox {label} contains a NUL byte")),
            })
        } else {
            Ok(())
        }
    };
    reject_nul("entry program", &spec.program)?;
    for (index, argument) in spec.args.iter().enumerate() {
        reject_nul(&format!("argument {index}"), argument)?;
    }
    if let Some(cwd) = &spec.cwd {
        reject_nul("working directory", cwd.as_os_str())?;
    }
    let cwd = match &spec.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().map_err(|error| Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some(format!(
                "resolving inherited sandbox working directory: {error}"
            )),
        })?,
    };
    let canonical = std::fs::canonicalize(&cwd).map_err(|error| Degradation {
        lost: vec!["process-cwd".to_string()],
        reason: Some(format!(
            "resolving sandbox working directory {}: {error}",
            cwd.display()
        )),
    })?;
    if !canonical.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return Err(Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some(format!(
                "sandbox working directory is not a directory: {}",
                canonical.display()
            )),
        });
    }
    for (key, value) in &policy.env.constructed {
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err(Degradation {
                lost: vec!["env".to_string()],
                reason: Some(format!("invalid target environment key: {key:?}")),
            });
        }
        if value.contains('\0') {
            return Err(Degradation {
                lost: vec!["env".to_string()],
                reason: Some(format!(
                    "target environment variable {key:?} contains a NUL byte"
                )),
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn os_str_contains_nul(value: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().contains(&0)
}

#[cfg(windows)]
fn os_str_contains_nul(value: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().any(|unit| unit == 0)
}

#[cfg(not(any(unix, windows)))]
fn os_str_contains_nul(value: &std::ffi::OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

/// Create the managed session tmp root for `TmpMode::Private` (else `None`). It lives with
/// the session lease, so every command submitted through that session sees the same private
/// location. Creation failure is a hard error: the engine never silently falls back to shared
/// tmp while claiming a private one.
fn make_private_tmp(policy: &SandboxPolicy) -> Result<Option<PrivateTemp>, Degradation> {
    // Windows acquires the stable profile-owned slot with its persistent native lease.
    if cfg!(windows) || policy.fs.tmp != crate::policy::TmpMode::Private {
        return Ok(None);
    }
    #[cfg(unix)]
    let created = PrivateTemp::new();
    #[cfg(not(unix))]
    let created = tempfile::Builder::new().prefix("nub-tmp-").tempdir();
    created.map(Some).map_err(|error| Degradation {
        lost: vec!["tmp-private".to_string()],
        reason: Some(format!("creating required private sandbox tmp: {error}")),
    })
}

/// Point a child's temp-dir env at `dir` (all three conventions: POSIX `TMPDIR`, the
/// `TMP`/`TEMP` pair Windows + many cross-platform tools read). Set AFTER `env_clear` so
/// it survives an enforced env scrub.
// Always defined, dead-code-allowed on Linux, for the same reason as `set_ca_env` above.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn set_tmp_env(command: &mut Command, dir: &std::path::Path) {
    for key in ["TMPDIR", "TMP", "TEMP"] {
        command.env(key, dir);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn teardown_reaps_descendants_after_the_leader_was_already_waited() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        struct GroupCleanup(i32);
        impl Drop for GroupCleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(-self.0, libc::SIGKILL);
                }
            }
        }
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & printf ready"])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let group = GroupCleanup(child.id() as i32);
        let mut output = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            output.read_to_end(&mut bytes).unwrap();
            let _ = tx.send(bytes);
        });
        assert!(child.wait().unwrap().success());
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        super::kill_and_reap(&mut child, Some(-group.0));
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        drop(group);
        reader.join().unwrap();
        assert_eq!(result.unwrap(), b"ready");
    }

    use super::*;

    #[test]
    fn proxy_activation_needs_an_explicit_mode_or_a_broker() {
        use crate::policy::{CredentialBroker, NetRule, NetTarget};

        let allow = NetRule {
            target: NetTarget::Host("api.example.com".to_string()),
            effect: Effect::Allow,
        };
        let mut policy = SandboxPolicy::default();

        // Coarse policy never starts a proxy, including unrestricted network.
        policy.net.enforce = false;
        policy.net.mode = ProxyMode::Auto;
        assert!(!proxy_needed(&policy));

        // An omitted proxy never turns a plain host allow into an in-path service.
        policy.net.enforce = true;
        policy.net.rules.push(allow.clone());
        policy.net.mode = ProxyMode::Disabled;
        assert!(!proxy_needed(&policy));

        // The author can request blind forwarding explicitly.
        policy.net.mode = ProxyMode::Auto;
        assert!(proxy_needed(&policy));

        // Credential injection is the narrow automatic exception: it needs a
        // terminating proxy even when the wrapper omitted `proxy`.
        policy.net.mode = ProxyMode::Disabled;
        policy.net.brokers.push(CredentialBroker {
            host: "api.example.com".to_string(),
            env: vec!["API_TOKEN".to_string()],
        });
        assert!(proxy_needed(&policy));
    }

    #[test]
    fn apply_validation_rejects_forged_broker_ir() {
        use crate::policy::{CredentialBroker, NetRule, NetTarget};

        let mut policy = SandboxPolicy::default();
        policy.env = crate::policy::EnvPolicy::resolved(Default::default());
        policy.net.enforce = true;
        policy.net.inspection = Inspection::TlsInspect;
        policy.net.rules.push(NetRule {
            target: NetTarget::Host("*".to_string()),
            effect: Effect::Allow,
        });
        policy.net.brokers.push(CredentialBroker {
            host: "*".to_string(),
            env: vec!["API_TOKEN".to_string()],
        });
        let err = validate_apply_inputs(&policy, &CommandSpec::new("/usr/bin/true")).unwrap_err();
        assert_eq!(err.lost, vec!["credential-broker"]);

        policy.net.brokers[0].host = "api.example.com".to_string();
        policy.net.brokers[0].env = vec!["TOKEN".to_string(), "TOKEN".to_string()];
        let err = validate_apply_inputs(&policy, &CommandSpec::new("/usr/bin/true")).unwrap_err();
        assert_eq!(err.lost, vec!["credential-broker"]);

        policy.net.brokers[0].env = vec!["TOKEN".to_string()];
        policy.net.rules.push(NetRule {
            target: NetTarget::Host("api.example.com".to_string()),
            effect: Effect::Deny,
        });
        let err = validate_apply_inputs(&policy, &CommandSpec::new("/usr/bin/true")).unwrap_err();
        assert_eq!(err.lost, vec!["credential-broker"]);

        policy.net.brokers[0].host = "127.1".to_string();
        policy.net.brokers[0].env = vec!["TOKEN".to_string()];
        policy.net.rules.push(NetRule {
            target: NetTarget::Host("127.1".to_string()),
            effect: Effect::Allow,
        });
        let err = validate_apply_inputs(&policy, &CommandSpec::new("/usr/bin/true")).unwrap_err();
        assert_eq!(err.lost, vec!["credential-broker"]);
        policy.net.rules.pop();

        policy.net.brokers[0].host = "api.example.com".to_string();
        policy.net.rules.pop();
        policy.net.brokers[0].env = vec!["HTTPS_PROXY".to_string()];
        let err = validate_apply_inputs(&policy, &CommandSpec::new("/usr/bin/true")).unwrap_err();
        assert_eq!(err.lost, vec!["credential-broker"]);
    }
}
