//! Per-OS enforcement backends and the [`apply`] entry that turns a resolved
//! [`SandboxPolicy`] into a launch-ready child.
//!
//! The enforcement contract is FAIL-SAFE-WITH-DEGRADATION, not fail-open (ported
//! from the reviewed salvage `backend/mod.rs`): a backend NEVER silently drops an
//! axis it claimed to enforce. When a primitive is unavailable it records the
//! loss in [`Degradation`] so the caller surfaces a WARNING; a hard fail-closed
//! (a required axis unenforceable) is `Err(Degradation)`.
//!
//! BACKEND STATUS: macOS (Seatbelt, [`macos`]), Linux (Bubblewrap mount/PID views,
//! [`linux`]), and Windows (AppContainer LowBox, [`windows`]) are wired; any other
//! OS runs the env-scrub-only [`generic_apply`] skeleton — which constructs the
//! child env and reports fs/net as NOT enforced. Every path preserves the API shape
//! (`apply(policy, spec) -> Result<Prepared, Degradation>`) the future embedder
//! seam slots into.
//!
//! LAUNCH SEAM: every backend returns a [`Prepared`] plan whose command is private.
//! Callers launch through [`Prepared::spawn`], [`Prepared::status`], or
//! [`Prepared::output`], preserving startup verification and resource ownership.
//! Windows owns the full synchronous spawn lifecycle because NEITHER of its launches
//! can be a pre-built `std::process::Command`: the AppContainer one needs
//! `CreateProcessW` with `STARTUPINFOEX`/`SECURITY_CAPABILITIES` plus a Job Object,
//! the dedicated-account one needs `CreateProcessWithLogonW`, and BOTH need per-run
//! ACL grants torn down after the child exits. [`Prepared::status`] is the uniform
//! verb: macOS/Linux/skeleton delegate to the configured command; Windows runs
//! whichever launcher its plan names (setup → spawn → wait → RAII teardown).

use crate::policy::{Effect, Inspection, ProxyMode, SandboxPolicy};
use crate::proxy::mitm::{BrokerSession, MitmEngine, RuntimeCredentialBroker};
use crate::proxy::{EgressProxy, StaticDecider};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::process::Command;
use std::sync::Arc;

/// What `nub setup-sandbox --check` found: the report a human reads, plus the single fact a script
/// needs. `ready` answers exactly one question — can the sandbox enforce for THIS caller, right
/// now — which is narrower than "is the host set up": a Linux host can be fully installed while
/// the calling shell still lacks the group. Carrying it as a field is what lets every platform
/// map the same question onto the same exit status instead of a caller parsing prose.
pub struct StatusReport {
    pub text: String,
    pub ready: bool,
}

#[cfg(target_os = "macos")]
mod macos;

// `macos_setup` (the no-op macOS half of `nub setup-sandbox`) was dropped with the curated
// zero-privilege import (epic row 0.3). Seatbelt needs no host provisioning; the readiness
// probe that lived there is inlined at its one caller (`preflight`), which reads the stock
// Seatbelt entry point through this re-export.
#[cfg(target_os = "macos")]
pub(crate) use macos::SANDBOX_EXEC_PATH;

// NOT macOS-gated, unlike its siblings: only the `log show` call inside is, and compiling the
// module everywhere keeps its record parser under test on every platform's CI leg rather than the
// one runner that can also enforce Seatbelt.
pub mod macos_denials;

#[cfg(target_os = "linux")]
mod linux;
// The Linux half of the aube-scripts embedder seam (epic 4.1): install nub's build-jail
// confinement onto a caller-owned command, so the lifecycle jail runs on the shared engine.
#[cfg(target_os = "linux")]
pub use linux::{BuildJailConfinement, confine_build_jail_command};

// DROPPED with the bubblewrap teardown (epic 1.1): `linux_setup` (the privileged helper-account /
// AppArmor host-setup tier), `linux_monitor` (the retained PID-1 monitor), `linux_net_bridge` (the
// host-side net bridge), `linux_runtime_stage` (bubblewrap runtime pinning), and `linux_probe` (the
// bubblewrap host probe, along with `linux::single_level_bwrap_candidate_paths` /
// `validate_adjacent_resource_bundle` / `bwrap_candidate_paths` that fed it). The zero-privilege
// enforcement tier is Landlock (`linux_landlock`) plus the seccomp user-notify egress supervisor
// (`linux_supervisor`); `linux_grants` (the OS-agnostic mount/grant derivation) stays below.

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

/// The explicit startup/apply seam carries no platform runtime image in the zero-privilege
/// skeleton. The privileged retained-monitor `RuntimeCapability` was dropped with `linux_monitor`
/// (epic row 0.3); the seccomp user-notify supervisor that replaces it defines its own capability
/// in a later phase. Kept as a unit struct on every target so the [`apply_with_runtime`] seam and
/// its `lib.rs` re-export stay uniform.
#[derive(Debug, Clone, Default)]
pub struct RuntimeCapability;

pub fn earliest_bootstrap() -> std::io::Result<RuntimeCapability> {
    Ok(RuntimeCapability)
}

// The Windows AppContainer backend. Compiled on Windows (its real consumer) and
// under `test` on any host — so its OS-agnostic IR→plan derivation (grant carve,
// capability selection, dangerous-root guard) is unit-tested on the macOS dev host
// without a Windows machine (the FFI launcher itself stays `#[cfg(windows)]`).
#[cfg(any(target_os = "windows", test))]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::windows_publish_appcontainer_read;
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub use windows::{windows_leaf_grant_redundant, windows_object_traverse_ace};

// Publishing a nub-owned, AppContainer-readable copy of a tool tree the jail must RUN — the
// escape from writing an ACE where a standard user cannot. Same cfg as `windows`: the copy half is
// ordinary fs work and is tested on the dev host, only the ace needs Windows.
#[cfg(any(target_os = "windows", test))]
pub mod windows_jail_bin;

// The Windows dedicated-account + WFP backend (`windows_account`) was DROPPED with the curated
// zero-privilege import (epic row 0.3): it was the privileged second-principal tier, and the
// zero-privilege skeleton keeps only the unprivileged AppContainer backend (`windows`).

// The window-station / desktop ACE machinery the AppContainer backend needs (a USER32-importing
// child on a non-interactive station dies in loader init without it) — resurrected from the
// dropped `windows_account` module, which is where it happened to live (epic row 3.2).
#[cfg(target_os = "windows")]
mod windows_ace;

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

/// How the child's argument tail is spelled on the wire.
///
/// Windows has no argv: `CreateProcessW` takes ONE command-line string, and every
/// program decides for itself how to split it. Rust's encoder targets the
/// `CommandLineToArgvW` rules, which `cmd.exe` does NOT implement — it treats `\"` as
/// two literal characters, so a script carrying interior quotes
/// (`node -e "require('is-odd')(3)"`) arrives mangled. aube therefore encodes the
/// `cmd.exe` line itself with `CommandExt::raw_arg` (see `spawn_shell_with_settings` in
/// `aube-scripts`); [`Verbatim`](Self::Verbatim) is how that already-encoded line
/// survives the trip through this crate to `CreateProcessW` instead of being
/// re-encoded into a line `cmd.exe` cannot parse.
///
/// NOT a general "skip the quoting" escape hatch: [`validate_apply_inputs`] refuses a
/// `Verbatim` tail off Windows, and refuses one whose program is not the Windows
/// command interpreter — the only program nub launches that parses its own line. Every
/// other spawn stays [`Argv`](Self::Argv) with byte-identical quoting to before.
#[derive(Debug, Clone)]
pub enum CommandArgs {
    /// Ordinary argv: each element is ONE argument, quoted by the launcher.
    Argv(Vec<std::ffi::OsString>),
    /// A pre-encoded Windows command-line TAIL, appended after the program name and
    /// handed to `CreateProcessW` byte-for-byte. Windows-only; see the type doc.
    Verbatim(std::ffi::OsString),
}

impl Default for CommandArgs {
    fn default() -> Self {
        Self::Argv(Vec::new())
    }
}

impl CommandArgs {
    /// The argv elements, or the whole verbatim line as a single item — the shape the
    /// NUL scan wants, where "which token" only matters for the error text.
    fn tokens(&self) -> impl Iterator<Item = &std::ffi::OsStr> {
        match self {
            Self::Argv(v) => Box::new(v.iter().map(std::ffi::OsString::as_os_str))
                as Box<dyn Iterator<Item = &std::ffi::OsStr>>,
            Self::Verbatim(line) => Box::new(std::iter::once(line.as_os_str())),
        }
    }


    /// Apply to a plain `std::process::Command` (the paths that spawn without a custom
    /// `CreateProcessW`).
    pub(crate) fn apply_to(&self, command: &mut Command) {
        match self {
            Self::Argv(v) => {
                command.args(v);
            }
            Self::Verbatim(line) => {
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.raw_arg(line);
                }
                #[cfg(not(windows))]
                {
                    let _ = line;
                    debug_assert!(
                        false,
                        "a verbatim command line is rejected off Windows by validate_apply_inputs"
                    );
                }
            }
        }
    }
}

/// The command to launch under a policy. Host-provided (Boundary B).
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: std::ffi::OsString,
    pub args: CommandArgs,
    /// Working directory for the child, if the caller pins one.
    pub cwd: Option<std::path::PathBuf>,
    /// Directories whose existing immediate children may be materialized for
    /// bounded deny globs such as `.env*` and `*.sandbox.json`. The frontend adds
    /// the workspace root and each package root; no backend recursively walks them.
    pub deny_search_roots: Vec<std::path::PathBuf>,
    /// This launch must be able to create nested sandboxes. On Linux under an
    /// AppArmor-restricted host that forces the dedicated administrator-installed
    /// helper as the level-1 launcher (a stock outer cannot transition to the
    /// helper's profile under `no_new_privs`) and fails the launch closed when the
    /// host is not set up for nesting. Default `false` leaves single-level launch
    /// candidate selection unchanged.
    pub require_nesting: bool,
    /// Pipe the child's stdout so the host can stream it through a redactor before
    /// forwarding. ONLY the request crosses this boundary — never the secret values
    /// (the host holds those and does the scrub). Default `false` = inherit (today's
    /// behavior, byte-for-byte).
    pub redact_stdout: bool,
    /// Pipe the child's stderr for host-side redaction. See [`redact_stdout`](Self::redact_stdout).
    pub redact_stderr: bool,
    /// Put the child in its OWN process group so its whole descendant tree can be
    /// signalled as `-pgid`. For third-party code the host cannot supervise any other way:
    /// a lifecycle shell's `node-gyp` → `make` → `cc` are unreachable from its pid, so
    /// killing the shell alone leaves them reparented to init and still writing.
    ///
    /// THE CANONICAL ACCOUNT OF WHO REAPS WHAT — the other sites reference this one. Each
    /// backend already has a mechanism except macOS: bubblewrap reaps through its PID
    /// namespace, Landlock through a group it takes unconditionally (its `setsid` is also
    /// its terminal hardening), the Windows AppContainer through a `KILL_ON_JOB_CLOSE` job
    /// object the child is assigned to while still suspended. So this flag is honored by the
    /// macOS backend alone, and as an OPT-IN rather than a default: leaving nub's process group
    /// is what forfeits a terminal Ctrl-C by membership, so only a caller that arranges its
    /// own signal reach should pay for it. Default `false` = today's behavior, byte-for-byte.
    ///
    /// A REQUEST, not a guarantee. A macOS launch with nothing to confine emits no profile
    /// and no hook, so it silently declines; `Prepared::spawn_with_signal_target` declines
    /// again if the kernel does not confirm the group, and warns when it does.
    pub reap_descendants: bool,
    /// A label naming THIS launch in the kernel's own denial records, so a failed script can be
    /// told what the jail refused instead of only that it failed.
    ///
    /// macOS ONLY, and the mechanism is why: Seatbelt's `(with message …)` modifier rides the
    /// profile's `(deny default)` and the kernel echoes it verbatim on every denial the launch
    /// provokes, where an unprivileged reader retrieves it. Linux Landlock's audit channel needs
    /// kernel 6.15 plus audit privilege, and Windows LowBox Permissive Learning Mode needs
    /// administrator AND stops enforcing — neither has an unprivileged twin, so those backends
    /// ignore this field. Retrieval: [`macos_denials`](crate::macos_denials).
    ///
    /// UNIQUE PER LAUNCH or it is wrong, not merely imprecise: the retrieval predicate IS this
    /// string, so two concurrent launches sharing one label cross-attribute each other's denials.
    pub audit_label: Option<String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            program: program.into(),
            args: CommandArgs::default(),
            cwd: None,
            deny_search_roots: Vec::new(),
            require_nesting: false,
            redact_stdout: false,
            redact_stderr: false,
            reap_descendants: false,
            audit_label: None,
        }
    }
    pub fn arg(mut self, a: impl Into<std::ffi::OsString>) -> Self {
        self.argv_mut().push(a.into());
        self
    }
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        self.argv_mut().extend(args.into_iter().map(Into::into));
        self
    }
    /// Hand the launcher a command line the caller has ALREADY encoded for the
    /// program's own parser, bypassing argv quoting. Replaces the whole tail — the two
    /// shapes are alternatives, never mixed. Accepted only for a Windows `cmd.exe`
    /// launch; see [`CommandArgs`] for why, and [`validate_apply_inputs`] for the gate.
    pub fn verbatim_command_line(mut self, line: impl Into<std::ffi::OsString>) -> Self {
        self.args = CommandArgs::Verbatim(line.into());
        self
    }
    fn argv_mut(&mut self) -> &mut Vec<std::ffi::OsString> {
        if let CommandArgs::Verbatim(_) = self.args {
            debug_assert!(
                false,
                "arg()/args() after verbatim_command_line() discards the encoded line"
            );
            self.args = CommandArgs::Argv(Vec::new());
        }
        match &mut self.args {
            CommandArgs::Argv(v) => v,
            CommandArgs::Verbatim(_) => unreachable!("converted to argv above"),
        }
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
    pub fn require_nesting(mut self, require: bool) -> Self {
        self.require_nesting = require;
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
    pub fn audit_label(mut self, label: impl Into<String>) -> Self {
        self.audit_label = Some(label.into());
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
    /// Files whose descriptors the Landlock backend consumes after fork (the ruleset fd its
    /// `pre_exec` hook restricts against). Keeping them here guarantees they remain open until
    /// `command` is spawned.
    #[cfg(target_os = "linux")]
    pub(crate) _inherited_files: Vec<std::fs::File>,
    /// Signal and reap the child's whole PROCESS GROUP rather than just the child.
    ///
    /// Set by the two paths whose `pre_exec` makes the child its own group leader: the
    /// Landlock build jail (`setsid`) and the macOS Seatbelt wrap when the caller asked
    /// for [`CommandSpec::reap_descendants`] (`setpgid(0, 0)`). It is the only handle
    /// either has on descendants — neither runs a PID-1 monitor, so without it a Ctrl-C
    /// reaches the script but not the compiler it spawned, and a normal return leaves
    /// that compiler writing into a package dir the installer already reported on.
    ///
    /// A REQUEST, not a fact: [`Prepared::spawn_with_signal_target`] downgrades it when
    /// the kernel does not confirm the child leads its own group. MUST stay false for
    /// every other path, where the child shares nub's own process group and a negative
    /// target would signal nub itself.
    #[cfg(unix)]
    pub(crate) signal_process_group: bool,
    /// Windows launch plan — the backend owns spawn+wait+teardown when this is `Some`.
    /// A two-variant enum, one per Windows mechanism: the per-run AppContainer (LowBox
    /// token) and the dedicated local account (`CreateProcessWithLogonW`, no LowBox
    /// token). Absent (or on other OSes) → [`Prepared::status`] spawns `command`.
    #[cfg(target_os = "windows")]
    pub(crate) launch: Option<windows::WindowsLaunch>,
    /// The fresh per-run PRIVATE tmp dir (`TmpMode::Private`), owned here so it lives for
    /// the child's whole run and is removed when `Prepared` drops (after the child exits).
    /// `None` for `Shared`/`Deny`. Held only for its Drop — the backends read its PATH via
    /// the value threaded into their `apply` before it moves here.
    pub(crate) _private_tmp: Option<tempfile::TempDir>,
    /// Pipe stdout/stderr at spawn so the host can drain them through an output redactor.
    /// Copied from [`CommandSpec`] in [`apply_inner`], applied in
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
    pub(crate) setsid: bool,
}

#[cfg(target_os = "linux")]
impl SupervisedPlan {
    /// Fork the confined child, run it under its supervisor, and return its exit status.
    fn run(self) -> std::io::Result<std::process::ExitStatus> {
        let SupervisedPlan {
            egress,
            argv,
            envp,
            cwd,
            ruleset,
            seccomp_ceiling,
            setsid,
        } = self;
        let launch = linux_supervisor::SupervisedLaunch {
            argv: &argv,
            envp: &envp,
            cwd: cwd.as_deref(),
            ruleset_fd: ruleset
                .as_ref()
                .map_or(-1, linux_landlock::LandlockRuleset::as_raw_fd),
            seccomp_ceiling: seccomp_ceiling.as_deref(),
            setsid,
        };
        let mut child = linux_supervisor::spawn_supervised(egress, launch)?;
        // Keep the ruleset alive across the fork+exec, exactly as the `Command` path keeps
        // `_inherited_files`: the child's `restrict_self` consumes the fd after fork.
        let status = child.wait();
        drop(ruleset);
        status
    }
}

/// A running prepared child together with every resource that must outlive it.
/// Dropping the handle kills and reaps the child before releasing those resources.
pub struct PreparedChild {
    child: Option<std::process::Child>,
    child_id: u32,
    #[cfg(unix)]
    signal_target: Option<i32>,
    #[cfg(unix)]
    signal_process_group: bool,
    _proxy: Option<EgressProxy>,
    _private_tmp: Option<tempfile::TempDir>,
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

    /// The child's process GROUP id — `Some` only when this launch put the child in its
    /// own group AND the kernel confirmed it leads one, which is the only state in which
    /// `-pgid` names the child's tree and nothing else.
    ///
    /// Exposed so a host can register the group with its own terminate-signal reaper: a
    /// signal whose default action kills nub never runs this handle's `Drop`, so the
    /// group would otherwise survive nub itself. `None` on every other path — the child
    /// shares nub's group there and signalling `-pgid` would kill nub and every sibling.
    #[cfg(unix)]
    pub fn process_group_id(&self) -> Option<i32> {
        self.signal_process_group.then_some(self.child_id as i32)
    }

    /// Take the piped stdout handle (present only when the launch requested
    /// `redact_stdout`). The host drains it through its output redactor. `None` when
    /// stdout was inherited or already taken.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// Take the piped stderr handle. See [`take_stdout`](Self::take_stdout).
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut().and_then(|c| c.stderr.take())
    }

    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
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
            if self.signal_process_group {
                unsafe { libc::kill(-(self.child_id as i32), libc::SIGKILL) };
            }
            self.child.take();
            self.release_resources();
        }
        result
    }

    pub fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        use std::io::Read;

        let child = self
            .child
            .as_mut()
            .ok_or_else(|| prepared_child_reaped_error("capture output from"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
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
        // Drop order matters: the proxy before the private tmp dir it may have written into.
        self._proxy.take();
        self._private_tmp.take();
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

/// Confirm the just-spawned child leads its OWN process group, which is what makes `-pid`
/// name its tree and nothing else.
///
/// THE GATE IS THE SAFETY PROPERTY. If the `pre_exec` hook's `setpgid`/`setsid` failed the
/// child is still in nub's group, where a negative target would SIGKILL nub itself along
/// with every sibling script — so the group is signalled on the kernel's word, never on the
/// assumption that the hook ran.
///
/// A PURE READ, deliberately: it never puts the child INTO a group. Completing the textbook
/// race-free `setpgid` pair from the parent would be both dead and dangerous — dead because
/// `pre_exec` forces std off `posix_spawn` onto fork+exec, where the parent blocks on the
/// exec-report pipe until the child has exec'd, so the hook has always run by the time
/// `spawn` returns; dangerous because it would MANUFACTURE the leadership this then
/// "confirms", turning any future launch that sets the flag without installing the hook
/// into a silent relocate-and-kill of an unrelated process.
///
/// `ESRCH` counts as confirmation, and is not the fail-open it looks like: macOS `getpgid`
/// refuses a ZOMBIE (measured; Linux answers correctly), so a script that exits the instant
/// it is spawned would otherwise disable reaping in precisely the case it exists for — the
/// shell gone, the writer it backgrounded running on. Safe because the hook provably ran (a
/// failing `pre_exec` fails the spawn instead) and the pid cannot be recycled while the
/// caller still holds the unreaped `Child`, so `-pid` names that child's own group or
/// nothing. The `getpgrp` check keeps that reasoning from being the only thing between a
/// pid and nub's own group.
#[cfg(unix)]
fn confirm_group_leader(pid: i32) -> bool {
    // SAFETY: `getpgid` on a child of this process, `getpgrp` on ourselves — plain reads.
    unsafe {
        if pid == libc::getpgrp() {
            return false;
        }
        match libc::getpgid(pid) {
            -1 => std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            pgid => pgid == pid,
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

    /// Whether this launch confines via the Windows AppContainer path, which owns
    /// spawn+wait internally ([`status`](Self::status) → `launch.run()`) and cannot
    /// hand back piped stdio. The host uses this to decide between the piped
    /// spawn+drain path and the opaque `status()` path when redacting on Windows.
    #[cfg(target_os = "windows")]
    pub fn will_confine(&self) -> bool {
        self.launch.is_some()
    }

    /// Install a signal target while a supervised Linux child is still blocked.
    #[doc(hidden)]
    pub fn spawn_with_signal_target(
        mut self,
        ready: impl FnOnce(PreparedSignalTarget) -> std::io::Result<()>,
    ) -> std::io::Result<PreparedChild> {
        #[cfg(target_os = "windows")]
        if self.launch.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "asynchronous confined Windows launches are not available",
            ));
        }
        // The supervised launch forks directly (the listener-fd barrier cannot ride
        // `Command::spawn`), so it has no `std::process::Child` to hand back — it is reachable
        // only through the synchronous `status()`. Refuse rather than spawn `command`
        // unsupervised, which would drop the net boundary the policy required.
        #[cfg(target_os = "linux")]
        if self.supervised.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "asynchronous supervised Linux launches are not yet available; use status()",
            ));
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
        #[allow(unused_mut)]
        let mut child = self.command.spawn()?;
        // The Landlock backend inherits its ruleset descriptor across spawn; the parent copy
        // can close the moment the child holds it (the `pre_exec` hook consumes it after fork).
        #[cfg(target_os = "linux")]
        self._inherited_files.clear();
        // A REQUEST until the kernel confirms it. `confirm_group_leader` is what turns it
        // into a fact, and everything downstream — the negative signal target, the reap in
        // `wait`, the pgid handed to the host — keys on the confirmed value.
        #[cfg(unix)]
        let signal_process_group =
            self.signal_process_group && confirm_group_leader(child.id() as i32);
        #[cfg(unix)]
        if self.signal_process_group && !signal_process_group {
            tracing::warn!(
                "sandbox: the confined child did not become its own process-group leader; \
                 running without descendant reaping — build tools it spawns may be orphaned \
                 and keep writing after this command returns"
            );
        }
        // Negative = the whole process group, and only ever after `confirm_group_leader`; see
        // `signal_process_group`. The retained-monitor launch that used to fork here was removed
        // with `linux_monitor` (epic 1.1); the Landlock path signals its child's group directly.
        #[cfg(unix)]
        let signal_target = {
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
            child_id,
            #[cfg(unix)]
            signal_target,
            #[cfg(unix)]
            signal_process_group,
            _proxy: self.proxy.take(),
            _private_tmp: self._private_tmp.take(),
        })
    }

    /// Launch the prepared child and wait for it, returning its exit status. The
    /// UNIFORM launch verb across backends: mac/linux/skeleton spawn `command`;
    /// Windows runs whichever launcher its plan names when one is attached — both
    /// follow the same shape (ACL setup → a custom spawn → wait → RAII teardown), and
    /// differ in the spawn: `CreateProcessW` under a LowBox token for the AppContainer,
    /// `CreateProcessWithLogonW` as the dedicated account for the other.
    ///
    /// The egress proxy (`self.proxy`) is held for the child's whole run and dropped
    /// (listener shut down) only after the child exits — `self` owns it until this
    /// method returns.
    #[allow(unused_mut)]
    pub fn status(mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(target_os = "windows")]
        if let Some(launch) = self.launch.take() {
            return launch.run();
        }
        // The Linux supervised launch owns its own fork+wait (the connect-notifier supervisor
        // runs in this process for the child's whole life), the same way the Windows launch does.
        #[cfg(target_os = "linux")]
        if let Some(plan) = self.supervised.take() {
            return plan.run();
        }
        let mut child = self.spawn()?;
        child.wait()
    }

    /// Launch, wait, and capture stdout/stderr through the supervised seam.
    pub fn output(mut self) -> std::io::Result<std::process::Output> {
        #[cfg(target_os = "windows")]
        if self.launch.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "captured confined Windows output is not available",
            ));
        }
        self.command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
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

fn start_proxy_if_needed(
    policy: &SandboxPolicy,
    runtime_brokers: Vec<RuntimeCredentialBroker>,
) -> Result<Option<EgressProxy>, Degradation> {
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
    // start_in_range carries #561's WFP loopback-window binding; the fail-closed
    // Result mapping is the epic branch's posture and is what `proxy_needed`'s doc
    // promises — a required proxy that fails to start is an apply error, never a
    // silent `None` that would launch the child with no egress mediation.
    EgressProxy::start_in_range(decider, mitm, proxy_port_range(policy))
        .map(Some)
        .map_err(|error| Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(format!("starting required egress proxy: {error}")),
        })
}

/// The loopback window the proxy must bind inside, when one applies. Always `None` now: the only
/// backend that needed a fixed window was Windows' dedicated-account + WFP tier (its elevated
/// setup pre-authorized one port range, so the proxy bound into it rather than a filter chasing
/// an ephemeral port), and that tier was dropped with the curated import (epic row 0.3). Every
/// surviving backend carves the exact proxy port at launch, so the proxy is free to take an
/// ephemeral one. Kept as a seam because Phase 5.1's per-host proxy may reintroduce a window.
fn proxy_port_range(_policy: &SandboxPolicy) -> Option<(u16, u16)> {
    None
}

/// The CA-trust env keys pointed at the child CA bundle (ephemeral CA + real roots).
/// A union of the common tool conventions — `NODE_EXTRA_CA_CERTS` is ADDITIVE (Node
/// keeps its built-in roots); the rest REPLACE the store, which is exactly why the bundle
/// carries the real roots alongside the CA. Brand-clean: every key is a tool's own
/// documented convention, none nub's. Set AFTER `env_clear` so it survives the scrub.
// ⛔ ALWAYS DEFINED, DEAD-CODE-ALLOWED ON LINUX — matching `set_proxy_env` below, which is the
// idiom this file already uses and the one these two deviated from. `mod windows` is declared
// `#[cfg(any(target_os = "windows", test))]` so Windows logic stays testable without a Windows
// machine, which means a LINUX TEST BUILD compiles `windows.rs` — and `windows.rs` calls this.
// Gating on `not(linux)` configured it out exactly there: E0425 on Linux, invisible on macOS where
// `not(linux)` is already true. Surfaced by a remote Linux build, never by a local gate.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn set_ca_env(command: &mut Command, bundle: &std::path::Path) {
    let path = bundle.as_os_str();
    for key in [
        "NODE_EXTRA_CA_CERTS", // Node (additive)
        "SSL_CERT_FILE",       // OpenSSL / curl / most
        "REQUESTS_CA_BUNDLE",  // python-requests
        "CURL_CA_BUNDLE",      // curl
        "GIT_SSL_CAINFO",      // git
        "PIP_CERT",            // pip
        "NPM_CONFIG_CAFILE",   // npm
        "npm_config_cafile",   // npm (lowercase form)
        "CARGO_HTTP_CAINFO",   // cargo
        "AWS_CA_BUNDLE",       // aws-cli
        "DENO_CERT",           // deno
    ] {
        command.env(key, path);
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

/// The cooperative proxy-env hint set on the child so ordinary HTTP(S) clients route
/// through the loopback proxy. NOT the boundary (a malicious client ignores it — the
/// OS deny-layer forces the traffic through); numeric host so the child needs no name
/// resolution. Both upper/lower case (tools split on which they read).
///
/// The per-session `token` is embedded as the URL userinfo (`http://<token>@127.0.0.1:
/// <port>`), so proxy-honoring clients send it as `Proxy-Authorization: Basic` (SOCKS
/// clients as RFC-1929 user-pass) automatically — the proxy rejects any handshake that
/// lacks it, closing the tokenless-egress-borrow. The token is the CHILD's own (it is
/// meant to have it); the point is that OTHER same-user processes do not. A `None` token
/// (defensive — should not occur when a proxy is running) yields a credential-less URL
/// the proxy will reject, i.e. fail-safe over-confinement, never a bypass.
///
/// `NODE_USE_ENV_PROXY=1` makes Node 24+ global `fetch` (undici) honor these proxy env
/// vars — without it a bare `fetch()` tries a direct connect the deny-layer blocks
/// (fail-closed but broken), instead of routing through the loopback proxy. Harmless
/// (ignored) on older Node. Internal nub-set plumbing var — brand-clean.
///
/// Whenever a proxy is engaged nub owns the child's ENTIRE proxy configuration, which is
/// why the bypass keys are cleared rather than left alone: the env allowlists admit an
/// ambient `NO_PROXY` and `npm_config_proxy` (a build script legitimately needs proxy
/// settings), and either one silently wins over what is set here — the fetcher direct-dials,
/// the deny layer blocks it, and the failure surfaces inside the dependency's own downloader
/// with nothing pointing at the sandbox. Clearing them costs the child nothing: the ambient
/// proxy host is unreachable from inside the confinement either way.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn set_proxy_env(command: &mut Command, port: u16, token: Option<&str>) {
    let url = match token {
        Some(t) => format!("http://{t}@127.0.0.1:{port}"),
        None => format!("http://127.0.0.1:{port}"),
    };
    for key in PROXY_URL_KEYS {
        command.env(key, &url);
    }
    for key in PROXY_BYPASS_KEYS {
        command.env_remove(key);
    }
    command.env("NODE_USE_ENV_PROXY", "1");
}

/// Keys pointed at the loopback proxy. The standard env set in both cases (tools split on
/// which they read), plus npm's own config spellings, which npm prefers over `HTTP_PROXY`.
const PROXY_URL_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "ALL_PROXY",
    "npm_config_proxy",
    "npm_config_https_proxy",
];

/// Keys that would route a request AROUND the proxy, and so must not survive into the
/// child. See [`set_proxy_env`] for why leaving them is a silent bypass.
const PROXY_BYPASS_KEYS: &[&str] = &["NO_PROXY", "no_proxy", "npm_config_noproxy"];

/// Point every proxy variable at a closed loopback port, for a net-DENIED child the OS is not
/// confining. Same move `net_gate_shim.js` makes on the children it spawns, hoisted to the
/// top-level process so it reaches the case the preload structurally cannot: a lifecycle entry
/// that is not Node at all.
///
/// ⛔ CALLERS MUST GATE ON A DENY. There is no permitted host to break under one, which is what
/// makes clobbering the whole proxy configuration safe; under an ALLOW this would break every
/// download the grant exists to permit. The shim expresses the same precondition by returning
/// early on `allow: true` before it ever builds its blackhole.
///
/// ADDITIVE, NOT A BOUNDARY, and the residual is the same one `net_gate_node_options` names:
/// `curl --noproxy '*'`, a static binary, or any client that does not read proxy env sails past
/// it. Verified against real `curl` on the host — a request that returns 200 unset fails
/// `connect to 127.0.0.1 port 1` with it set.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn set_proxy_blackhole(command: &mut Command) {
    for key in PROXY_URL_KEYS {
        command.env(key, "http://127.0.0.1:1");
    }
    for key in PROXY_BYPASS_KEYS {
        command.env_remove(key);
    }
}

/// Apply a resolved policy to a command, dispatching to the per-OS backend.
///
/// The env axis is enforced by CONSTRUCTION (not an OS primitive): when the policy
/// enforces env, the child env is cleared and set to exactly the policy's
/// constructed map. Each OS backend additionally hardens a scrubbed env so the
/// withheld secret can't be recovered from a co-resident same-uid process: Linux via
/// a fresh PID namespace and procfs (the host parent is not visible); macOS via the
/// Seatbelt env-read closure
/// (`deny process-info*` + self-restore, which shuts the `KERN_PROCARGS2` argv/env
/// read). Because the macOS closure lives in the SBPL profile, a policy that withholds
/// a secret is wrapped even when fs/net are relaxed (see `macos::needs_wrap`). fs/net
/// enforcement is the backend's job; on an OS whose backend has not landed,
/// [`generic_apply`] reports them as not-enforced (never silent).
pub fn apply(policy: &SandboxPolicy, spec: CommandSpec) -> Result<Prepared, Degradation> {
    apply_inner(policy, spec, None)
}

/// Apply with the verified runtime capability returned by [`earliest_bootstrap`].
/// Linux confinement requires this explicit embedder seam; other platforms accept
/// the value and retain their existing launch behavior.
pub fn apply_with_runtime(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    runtime: &RuntimeCapability,
) -> Result<Prepared, Degradation> {
    apply_inner(policy, spec, Some(runtime))
}

fn apply_inner(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    runtime: Option<&RuntimeCapability>,
) -> Result<Prepared, Degradation> {
    // The Linux confinement path no longer consumes the embedder runtime capability: the
    // retained-monitor tier that materialized a runtime image was dropped with `linux_monitor`
    // (epic 1.1). The seam stays on the public API for the supervisor phase (epic 1.1d).
    let _ = runtime;
    if !policy.env.resolved {
        return Err(Degradation {
            lost: vec!["env-unresolved".to_string()],
            reason: Some(
                "sandbox policy has no resolved target environment; compile it with an ambient snapshot before apply"
                    .to_string(),
            ),
        });
    }
    validate_apply_inputs(policy, &spec)?;
    // Captured before the per-OS backend consumes `spec`; re-applied to the returned
    // `Prepared` below (ONE place) so every backend inherits the stdio-redaction request
    // without threading the boolean through each `apply`.
    let redact_stdout = spec.redact_stdout;
    let redact_stderr = spec.redact_stderr;
    let mut runtime_policy = policy.clone();
    let runtime_brokers = if policy.net.brokers.is_empty() {
        Vec::new()
    } else {
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
        session.into_brokers()
    };
    let policy = &runtime_policy;
    #[cfg(target_os = "linux")]
    let linux_preflight = linux::preflight(policy, &spec)?;
    // Start the per-host egress proxy FIRST (if the policy needs it), so its bound port
    // is threaded into the backend deny-layer (which permits egress ONLY to the proxy
    // endpoint) before the child is prepared. The proxy is then stashed on `Prepared`
    // so it outlives the child (design.md §2.5).
    // THE LANDLOCK ARM STARTS NO PROXY. It has no netns, so a child cannot be routed through one
    // — its net axis is a coarse per-package seccomp family permit (`linux::apply_landlock`), and
    // an Allow rule there is provenance rather than a host gate. Starting one anyway left a
    // loopback listener nothing could use on every catalogued package's lifecycle spawn, and a
    // bind failure is a HARD apply error, so the machinery could refuse an install it does not
    // participate in. A broker still forces the proxy: it is the only thing that can perform the
    // marker→secret swap, and skipping it would turn a grant into a silent failure rather than a
    // saved listener. No build-jail policy has brokers (its env axis strips the credential
    // family), so in production this predicate is just "the build jail".
    #[cfg(target_os = "linux")]
    let proxy = if linux_preflight.uses_landlock() && policy.net.brokers.is_empty() {
        None
    } else {
        start_proxy_if_needed(policy, runtime_brokers)?
    };
    #[cfg(not(target_os = "linux"))]
    let proxy = start_proxy_if_needed(policy, runtime_brokers)?;
    let proxy_port = proxy.as_ref().map(EgressProxy::port);
    // The per-session egress-proxy token, delivered to the child via the proxy URL. Same
    // presence as `proxy_port` (both derive from `proxy`), threaded into each backend so
    // the child authenticates to the loopback proxy.
    let proxy_token = proxy.as_ref().map(EgressProxy::token);
    // The Linux Landlock backend takes neither: it confines egress with a coarse seccomp family
    // ceiling and has no proxy to authenticate to. Both are threaded into the mac/win/generic
    // backends below, which are cfg'd out here. (epic 1.1d wires the supervisor into this seam.)
    #[cfg(target_os = "linux")]
    let _ = (proxy_port, proxy_token);
    // The child CA bundle, when TLS termination engaged — its ephemeral path, threaded into the
    // mac/win/generic backends. On Linux the only wired backend is the Landlock build jail, which
    // starts no proxy and terminates no TLS, so there is never a CA bundle to hand it or announce.
    #[cfg(not(target_os = "linux"))]
    let ca_bundle = proxy.as_ref().and_then(|p| p.ca_bundle_path());
    #[cfg(not(target_os = "linux"))]
    let ca_bundle_present = ca_bundle.is_some();
    #[cfg(target_os = "linux")]
    let ca_bundle_present = false;

    // Create the fresh per-run PRIVATE tmp dir up front (when the policy asks), so its
    // path is threaded into the backend BEFORE the child profile is built — the backend
    // grants it rw + points the child's TMPDIR at it + hides the shared system tmp. The
    // dir is owned by `Prepared` (moved in below) so it outlives the child and is removed
    // on drop. `None` for Shared/Deny.
    let private_tmp = make_private_tmp(policy);
    let tmp_dir = private_tmp.as_ref().map(|d| d.path());

    #[cfg(target_os = "macos")]
    let mut prepared = macos::apply(policy, spec, proxy_port, proxy_token, ca_bundle, tmp_dir)?;
    // The Linux backend takes neither the proxy nor a net bridge: the Landlock build jail confines
    // egress with a coarse seccomp family ceiling and has no netns to route through, and the
    // retained-monitor net bridge was dropped with `linux_monitor` (epic 1.1). The non-Landlock
    // seam that would drive `linux_supervisor` is epic 1.1(d).
    #[cfg(target_os = "linux")]
    let mut prepared = linux::apply(policy, spec, tmp_dir, linux_preflight)?;
    #[cfg(target_os = "windows")]
    let mut prepared = windows::apply(policy, spec, proxy_port, proxy_token, ca_bundle, tmp_dir)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let mut prepared = generic_apply(policy, spec, proxy_port, proxy_token, ca_bundle, tmp_dir)?;

    // One-line stderr notice when TLS termination ACTUALLY engages — never silent, but
    // never MISLEADING either: suppress it where the backend degraded net (e.g. Windows,
    // whose AppContainer child can't reach the loopback proxy, so termination never
    // happens and the request is fail-safe denied — announcing it would be a false claim).
    if ca_bundle_present
        && !prepared
            .degradation
            .lost
            .iter()
            .any(|l| l.starts_with("net-per"))
    {
        emit_mitm_notice(policy);
    }

    prepared.proxy = proxy;
    prepared._private_tmp = private_tmp;
    prepared.redact_stdout = redact_stdout;
    prepared.redact_stderr = redact_stderr;
    Ok(prepared)
}

/// Whether `program` names `cmd.exe` — the sole program whose command line nub hands
/// over verbatim. Matched on the file name so it holds for the bare name aube passes
/// and for an absolute `System32` path alike; case-insensitive because Windows paths
/// are. Deliberately NOT extended to `powershell`/`pwsh`: neither is on the lifecycle
/// spawn path, and each would need its own audited encoder before it could opt in.
fn program_is_windows_command_interpreter(program: &std::ffi::OsStr) -> bool {
    std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe") || n.eq_ignore_ascii_case("cmd"))
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
    for (index, argument) in spec.args.tokens().enumerate() {
        reject_nul(&format!("argument {index}"), argument)?;
    }
    if let CommandArgs::Verbatim(_) = spec.args {
        // The ONLY sanctioned verbatim caller is aube's `cmd.exe` script line, so the
        // opt-in is confined to exactly that shape rather than left open as a
        // skip-the-quoting hatch any future caller could reach for. Fail closed: a
        // verbatim tail anywhere else is a programming error, not a degradation to
        // absorb, and silently re-encoding it would reintroduce the original bug.
        if !cfg!(windows) {
            return Err(Degradation {
                lost: vec!["process-input".to_string()],
                reason: Some("a verbatim command line is a Windows-only encoding".to_string()),
            });
        }
        if !program_is_windows_command_interpreter(&spec.program) {
            return Err(Degradation {
                lost: vec!["process-input".to_string()],
                reason: Some(format!(
                    "a verbatim command line is only accepted for the Windows command \
                     interpreter, not {}",
                    std::path::Path::new(&spec.program).display()
                )),
            });
        }
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

/// Create the fresh per-run private tmp dir for `TmpMode::Private` (else `None`). A
/// `tempfile::TempDir` under the OS default temp root, removed when it drops (after the
/// child exits, since `Prepared` owns it). A creation failure yields `None` — the backend
/// then reports the tmp axis unenforced (fail-safe: it never silently runs the child on
/// the SHARED tmp while claiming a private one).
fn make_private_tmp(policy: &SandboxPolicy) -> Option<tempfile::TempDir> {
    if policy.fs.tmp != crate::policy::TmpMode::Private {
        return None;
    }
    tempfile::Builder::new().prefix("nub-tmp-").tempdir().ok()
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

/// Env-scrub-only skeleton for an OS with no wired backend. Reports fs and net as
/// not-enforced so a caller never mistakes the skeleton for confinement.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn generic_apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&std::path::Path>,
    tmp_dir: Option<&std::path::Path>,
) -> Result<Prepared, Degradation> {
    if !policy.net.brokers.is_empty() {
        return Err(Degradation {
            lost: vec!["credential-broker".to_string()],
            reason: Some(
                "this OS backend cannot force brokered traffic through the credential proxy"
                    .to_string(),
            ),
        });
    }
    let mut command = Command::new(&spec.program);
    spec.args.apply_to(&mut command);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }

    // Env axis — construction, not interception.
    command.env_clear();
    for (k, v) in &policy.env.constructed {
        command.env(k, v);
    }
    if let Some(port) = proxy_port {
        set_proxy_env(&mut command, port, proxy_token);
    }
    if let Some(bundle) = ca_bundle {
        set_ca_env(&mut command, bundle);
    }
    if let Some(dir) = tmp_dir {
        set_tmp_env(&mut command, dir);
    }

    // fs/net: honestly report what the skeleton does not yet enforce. The skeleton has
    // NO OS deny-layer, so even with the proxy running it cannot FORCE the child
    // through it — net is reported unenforced regardless.
    let mut lost = Vec::new();
    if fs_confines(policy) {
        lost.push("fs".to_string());
    }
    if policy.net.enforce {
        lost.push("net".to_string());
    }
    if let Some(axis) = tmp_lost_axis(policy) {
        lost.push(axis.to_string());
    }
    let degradation = if lost.is_empty() {
        Degradation::full()
    } else {
        Degradation {
            lost,
            reason: Some("no OS backend wired in this build (Stage 1)".to_string()),
        }
    };
    Ok(Prepared {
        command,
        degradation,
        proxy: None,
        #[cfg(target_os = "linux")]
        _inherited_files: Vec::new(),
        #[cfg(unix)]
        signal_process_group: false,
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

/// The degradation axis name for a backend that does NOT enforce the requested
/// [`TmpMode`] — `tmp-private` (a private per-run tmp was requested but the shared
/// system tmp is not hidden) / `tmp-deny` (tmp was to be denied but is not). `None` for
/// `Shared` (nothing to enforce). A backend that DOES enforce the mode never calls this;
/// one that doesn't pushes the axis into `lost` so the caller never mistakes an
/// unenforced private/deny-tmp for a real one (fail-safe honesty, never silent).
/// macOS ENFORCES the mode in its SBPL, so it never consults this (hence the cfg).
#[cfg(any(
    target_os = "windows",
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn tmp_lost_axis(policy: &SandboxPolicy) -> Option<&'static str> {
    match policy.fs.tmp {
        crate::policy::TmpMode::Shared => None,
        crate::policy::TmpMode::Private => Some("tmp-private"),
        crate::policy::TmpMode::Deny => Some("tmp-deny"),
    }
}

/// Whether the fs policy actually confines anything (a non-relaxed base or any
/// entry). A relaxed fs axis (allow-all, no rules) is not a lost enforcement.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn fs_confines(policy: &SandboxPolicy) -> bool {
    !matches!(policy.fs.rules.default_effect, crate::policy::Effect::Allow)
        || !policy.fs.rules.entries.is_empty()
}

#[cfg(test)]
mod tests {
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

    /// The verbatim command line exists for ONE caller — aube's already-encoded `cmd.exe`
    /// script tail — and must never become a general "skip argv quoting" hatch, which
    /// would let a future caller hand an arbitrary program an unquoted, attacker-shaped
    /// line. Both halves of the gate are asserted here rather than left to review.
    #[test]
    fn a_verbatim_command_line_is_confined_to_the_windows_command_interpreter() {
        let policy = SandboxPolicy::default();

        let spec = CommandSpec::new("node.exe").verbatim_command_line("-e \"boom\"");
        let err = validate_apply_inputs(&policy, &spec).unwrap_err();
        assert_eq!(err.lost, vec!["process-input"]);
        assert!(
            err.reason.as_deref().is_some_and(|r| r
                .contains("only accepted for the Windows command interpreter")
                || r.contains("Windows-only encoding")),
            "an off-interpreter verbatim line must be refused by name, got {:?}",
            err.reason
        );

        // The sanctioned shape: `cmd.exe` by bare name (what aube passes) and by absolute
        // path (what a resolved program would be), each accepted only on Windows.
        for program in ["cmd.exe", "CMD.EXE", r"C:\Windows\System32\cmd.exe"] {
            let spec = CommandSpec::new(program).verbatim_command_line("/d /s /c \" echo hi \"");
            let verdict = validate_apply_inputs(&policy, &spec);
            assert_eq!(
                verdict.is_ok(),
                cfg!(windows),
                "{program} verbatim acceptance must track the platform, got {verdict:?}"
            );
        }

        // Ordinary argv is untouched by the gate on every platform.
        let spec = CommandSpec::new("node.exe").args(["-e", "boom"]);
        assert!(validate_apply_inputs(&policy, &spec).is_ok());
    }

    /// The two shapes are alternatives, never a mix — a spec cannot carry an encoded line
    /// AND argv, because a launcher would have to guess which one the caller meant.
    #[test]
    fn setting_one_argument_shape_replaces_the_other() {
        let spec = CommandSpec::new("cmd.exe")
            .args(["-c", "ignored"])
            .verbatim_command_line("/d /s /c \" echo hi \"");
        match &spec.args {
            CommandArgs::Verbatim(line) => assert_eq!(line, "/d /s /c \" echo hi \""),
            other => panic!("expected the verbatim line to win, got {other:?}"),
        }
    }

    /// A bypass key the child inherited must not survive, or the whole per-host policy is
    /// advisory: the build jail's env allowlist admits `NO_PROXY` and `npm_config_proxy`
    /// (a build script legitimately configures a proxy), and a fetcher that honors either
    /// direct-dials past the loopback gate and dies against the deny layer instead.
    #[test]
    fn set_proxy_env_clears_the_bypass_keys_the_child_inherited() {
        let mut cmd = Command::new("true");
        cmd.env("NO_PROXY", "*")
            .env("no_proxy", "nodejs.org")
            .env("npm_config_noproxy", "*")
            .env("npm_config_proxy", "http://corp.example:8080");
        set_proxy_env(&mut cmd, 4321, Some("abc123"));
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        // Windows environment names are case-insensitive, so `NO_PROXY` and `no_proxy`
        // collapse into ONE `CommandEnv` entry keyed by whichever spelling arrived first —
        // an exact-name lookup misses the survivor and reads as "not removed". Fold the
        // lookup the way the platform folds the names.
        let entry = |key: &str| {
            envs.iter()
                .find(|(k, _)| {
                    if cfg!(windows) {
                        k.eq_ignore_ascii_case(key)
                    } else {
                        k.as_str() == key
                    }
                })
                .map(|(_, v)| v)
        };
        for key in PROXY_BYPASS_KEYS {
            assert_eq!(
                entry(key),
                Some(&None),
                "`{key}` must be removed from the child env, not merely left unset"
            );
        }
        assert_eq!(
            entry("npm_config_proxy").and_then(|v| v.as_deref()),
            Some("http://abc123@127.0.0.1:4321"),
            "npm reads its own config spelling first, so it must point at the loopback proxy"
        );
    }

    /// The blackhole must reach EVERY spelling and take the bypass keys with it — an ambient
    /// `NO_PROXY=*` surviving would restore direct egress for the whole child, which is the
    /// same silent bypass [`set_proxy_env`] clears for the proxy case.
    ///
    /// Port 1 is the mechanism, verified against real `curl` on the host rather than assumed:
    /// a request that returns 200 with no proxy env fails `connect to 127.0.0.1 port 1` with
    /// this applied.
    #[test]
    fn set_proxy_blackhole_points_every_spelling_at_a_closed_port() {
        let mut cmd = Command::new("true");
        cmd.env("NO_PROXY", "*")
            .env("npm_config_noproxy", "registry.npmjs.org")
            .env("HTTPS_PROXY", "http://corp.example:8080");
        set_proxy_blackhole(&mut cmd);
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        // Case-folded on Windows for the reason given above.
        let entry = |key: &str| {
            envs.iter()
                .find(|(k, _)| {
                    if cfg!(windows) {
                        k.eq_ignore_ascii_case(key)
                    } else {
                        k.as_str() == key
                    }
                })
                .map(|(_, v)| v)
        };
        for key in PROXY_URL_KEYS {
            assert_eq!(
                entry(key).and_then(|v| v.as_deref()),
                Some("http://127.0.0.1:1"),
                "`{key}` must point at the closed port, overwriting whatever the child inherited"
            );
        }
        for key in PROXY_BYPASS_KEYS {
            assert_eq!(
                entry(key),
                Some(&None),
                "`{key}` must be removed, or the blackhole is routed around"
            );
        }
    }

    /// The proxy env embeds the per-session token as the URL userinfo (so proxy-honoring
    /// clients authenticate automatically) and sets `NODE_USE_ENV_PROXY=1` so Node 24+
    /// global `fetch` routes through the loopback proxy rather than a direct-connect the
    /// deny-layer blocks.
    #[test]
    fn set_proxy_env_embeds_token_and_enables_node_env_proxy() {
        let mut cmd = Command::new("true");
        set_proxy_env(&mut cmd, 4321, Some("abc123"));
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("HTTP_PROXY").and_then(|v| v.as_deref()),
            Some("http://abc123@127.0.0.1:4321"),
            "the token must be the URL userinfo"
        );
        assert_eq!(
            envs.get("NODE_USE_ENV_PROXY").and_then(|v| v.as_deref()),
            Some("1"),
            "NODE_USE_ENV_PROXY must be set so Node 24+ fetch honors the proxy"
        );
    }

    /// Defensive: a missing token (should not occur when a proxy is live) yields a
    /// credential-less URL the proxy will reject — fail-safe over-confinement, not a
    /// tokenless bypass.
    #[test]
    fn set_proxy_env_without_token_is_credential_less() {
        let mut cmd = Command::new("true");
        set_proxy_env(&mut cmd, 4321, None);
        let url = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("HTTP_PROXY"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:4321"));
    }
}
