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
//! Windows owns the full synchronous spawn lifecycle because AppContainer creation
//! needs `CreateProcessW` with `STARTUPINFOEX`/`SECURITY_CAPABILITIES`, a Job Object,
//! and per-run ACL grants that are torn down after exit.

use crate::policy::{Effect, Inspection, ProxyMode, SandboxPolicy};
use crate::proxy::mitm::MitmEngine;
use crate::proxy::{EgressProxy, StaticDecider};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::process::Command;
use std::sync::Arc;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
mod linux_monitor;

#[cfg(target_os = "linux")]
pub use linux_monitor::{
    RuntimeCapability, earliest_bootstrap, exercise_monitor_state_6, exercise_monitor_state_7,
    exercise_monitor_state_8, exercise_monitor_states_1_to_5, exercise_nested_worker_reentry,
};

/// Non-Linux embedders keep the same explicit startup/apply seam without carrying
/// a platform runtime image.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Default)]
pub struct RuntimeCapability;

#[cfg(not(target_os = "linux"))]
pub fn earliest_bootstrap() -> std::io::Result<RuntimeCapability> {
    Ok(RuntimeCapability)
}

// The Windows AppContainer backend. Compiled on Windows (its real consumer) and
// under `test` on any host — so its OS-agnostic IR→plan derivation (grant carve,
// capability selection, dangerous-root guard) is unit-tested on the macOS dev host
// without a Windows machine (the FFI launcher itself stays `#[cfg(windows)]`).
#[cfg(any(target_os = "windows", test))]
mod windows;

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
}

impl CommandSpec {
    pub fn new(program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            deny_search_roots: Vec::new(),
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
}

/// A launch-ready child. The command stays private so backend supervision and
/// cleanup cannot be bypassed; callers launch through [`Prepared::spawn`],
/// [`Prepared::status`], or [`Prepared::output`].
pub struct Prepared {
    /// The configured child for the mac/linux/skeleton path. On Windows this is the
    /// env-scrubbed plain child used ONLY when nothing needs AppContainer confinement
    /// (`launch` is `None`); when confinement applies, `launch` owns the spawn and
    /// this field is unused.
    command: Command,
    pub degradation: Degradation,
    /// The running egress proxy (design.md §2.5), when the policy enforces per-host
    /// net. It runs in the nub PARENT and MUST outlive the child, so it is owned here:
    /// [`Prepared::status`] holds it for the child's whole run, and dropping this
    /// value stops the listener. `None` when net is unconfined or coarse-deny (no
    /// proxy needed). Set by [`apply`], not the per-OS backends.
    pub(crate) proxy: Option<EgressProxy>,
    /// Files whose descriptors Bubblewrap consumes while constructing the mount
    /// view (currently the empty regular-file source used for exact deny masks).
    /// Keeping them here guarantees they remain open until `command` is spawned.
    #[cfg(target_os = "linux")]
    pub(crate) _inherited_files: Vec<std::fs::File>,
    /// One-shot authority for the authenticated Linux PID-1 monitor launch.
    #[cfg(target_os = "linux")]
    pub(crate) retained_monitor: Option<linux_monitor::RetainedMonitorLaunch>,
    /// Windows AppContainer launch plan — the backend owns spawn+wait+teardown when
    /// this is `Some`. Absent (or on other OSes) → [`Prepared::status`] spawns
    /// `command`.
    #[cfg(target_os = "windows")]
    pub(crate) launch: Option<windows::WindowsLaunch>,
    /// The fresh per-run PRIVATE tmp dir (`TmpMode::Private`), owned here so it lives for
    /// the child's whole run and is removed when `Prepared` drops (after the child exits).
    /// `None` for `Shared`/`Deny`. Held only for its Drop — the backends read its PATH via
    /// the value threaded into their `apply` before it moves here.
    pub(crate) _private_tmp: Option<tempfile::TempDir>,
}

/// A running prepared child together with every resource that must outlive it.
/// Dropping the handle kills and reaps the child before releasing those resources.
pub struct PreparedChild {
    child: Option<std::process::Child>,
    child_id: u32,
    #[cfg(unix)]
    signal_target: Option<i32>,
    #[cfg(target_os = "linux")]
    retained_monitor: Option<linux_monitor::RetainedMonitorSession>,
    _proxy: Option<EgressProxy>,
    _private_tmp: Option<tempfile::TempDir>,
}

/// The signal destination authenticated during [`Prepared::spawn_with_signal_target`].
#[doc(hidden)]
pub enum PreparedSignalTarget {
    Direct(i32),
    #[cfg(target_os = "linux")]
    Callback(PreparedSignalCallback),
}

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub type PreparedSignalCallback =
    Arc<dyn Fn(libc::c_int) -> std::io::Result<()> + Send + Sync + 'static>;

impl PreparedChild {
    pub fn id(&self) -> u32 {
        self.child_id
    }

    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| prepared_child_reaped_error("wait"))?;
        #[cfg(target_os = "linux")]
        let result = match self.retained_monitor.as_mut() {
            Some(session) => session.wait(child),
            None => wait_child_eintr(child),
        };
        #[cfg(not(target_os = "linux"))]
        let result = wait_child_eintr(child);
        #[cfg(target_os = "linux")]
        let cleanup_complete = self
            .retained_monitor
            .as_ref()
            .is_some_and(linux_monitor::RetainedMonitorSession::cleanup_complete);
        #[cfg(not(target_os = "linux"))]
        let cleanup_complete = false;
        if result.is_ok() || cleanup_complete {
            self.child.take();
            #[cfg(target_os = "linux")]
            self.retained_monitor.take();
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
        #[cfg(target_os = "linux")]
        if let Some(mut session) = self.retained_monitor.take() {
            if let Err(error) = session.fail_closed(&mut child) {
                eprintln!("fatal: retained sandbox cleanup failed: {error}");
                std::process::abort();
            }
            return;
        }
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
        #[cfg(target_os = "windows")]
        if self.launch.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "asynchronous confined Windows launches are not available",
            ));
        }
        #[allow(unused_mut)]
        let mut child = self.command.spawn()?;
        // Bubblewrap inherited its setup-data descriptors across spawn. The parent
        // copies can close immediately; in particular, do not retain the serialized
        // target environment in the long-lived PreparedChild handle.
        #[cfg(target_os = "linux")]
        self._inherited_files.clear();
        #[cfg(target_os = "linux")]
        let (launched_child, child_id, signal_target, retained_monitor) =
            match self.retained_monitor.take() {
                Some(launch) => {
                    let (mut outer, mut session) = launch.start(child)?;
                    let child_id = session.target_pid() as u32;
                    if let Err(error) =
                        ready(PreparedSignalTarget::Callback(session.signal_callback()))
                    {
                        return match session.fail_closed(&mut outer) {
                            Ok(()) => Err(error),
                            Err(cleanup) => Err(std::io::Error::new(
                                error.kind(),
                                format!("{error}; retained sandbox cleanup also failed: {cleanup}"),
                            )),
                        };
                    }
                    (outer, child_id, None, Some(session))
                }
                None => {
                    let target = child.id() as i32;
                    if let Err(error) = ready(PreparedSignalTarget::Direct(target)) {
                        kill_and_reap(&mut child, Some(target));
                        return Err(error);
                    }
                    let child_id = child.id();
                    (child, child_id, Some(target), None)
                }
            };
        #[cfg(target_os = "linux")]
        let child = launched_child;
        #[cfg(all(unix, not(target_os = "linux")))]
        let signal_target = {
            let target = child.id() as i32;
            if let Err(error) = ready(PreparedSignalTarget::Direct(target)) {
                kill_and_reap(&mut child, Some(target));
                return Err(error);
            }
            Some(target)
        };
        #[cfg(not(unix))]
        let _ = ready;
        #[cfg(not(target_os = "linux"))]
        let child_id = child.id();
        Ok(PreparedChild {
            child: Some(child),
            child_id,
            #[cfg(unix)]
            signal_target,
            #[cfg(target_os = "linux")]
            retained_monitor,
            _proxy: self.proxy.take(),
            _private_tmp: self._private_tmp.take(),
        })
    }

    /// Launch the prepared child and wait for it, returning its exit status. The
    /// UNIFORM launch verb across backends: mac/linux/skeleton spawn `command`;
    /// Windows runs its AppContainer launcher (ACL setup → `CreateProcessW` under a
    /// LowBox token → wait → RAII teardown) when a launch plan is attached.
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

/// Whether the policy needs the per-host egress proxy: net enforced AND at least one
/// Allow rule (a pure deny-all is coarse — no proxy, nothing is reachable). A proxy
/// that fails to start degrades to coarse-deny (fail-SAFE: denies more, not less), so
/// this returns `None` on a start failure and the backend reports `net-per-host`.
fn start_proxy_if_needed(policy: &SandboxPolicy) -> Option<EgressProxy> {
    if !(policy.net.enforce && policy.net.rules.iter().any(|r| r.effect == Effect::Allow)) {
        return None;
    }
    let decider = Arc::new(StaticDecider::new(policy.net.clone()));
    let mitm = match policy.net.inspection {
        Inspection::TlsInspect => {
            let terminate_all = matches!(policy.net.mode, ProxyMode::Terminate);
            match MitmEngine::new(policy.net.brokers.clone(), terminate_all) {
                Ok(engine) => Some(engine),
                // FAIL-CLOSED: the tier required TLS termination but the CA/TLS stack
                // could not be built. Do NOT downgrade to a blind splice — that would
                // forward brokered requests UN-injected. Return None so the whole net
                // coarse-denies (fail-safe over-confine); the backend reports net lost.
                Err(_) => return None,
            }
        }
        Inspection::Connection => None,
    };
    EgressProxy::start(decider, mitm).ok()
}

/// The CA-trust env keys pointed at the child CA bundle (ephemeral CA + real roots).
/// A union of the common tool conventions — `NODE_EXTRA_CA_CERTS` is ADDITIVE (Node
/// keeps its built-in roots); the rest REPLACE the store, which is exactly why the bundle
/// carries the real roots alongside the CA. Brand-clean: every key is a tool's own
/// documented convention, none nub's. Set AFTER `env_clear` so it survives the scrub.
#[cfg(not(target_os = "linux"))]
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

#[cfg(target_os = "linux")]
fn insert_ca_env(env: &mut BTreeMap<OsString, OsString>, bundle: &std::path::Path) {
    for key in [
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
    ] {
        env.insert(OsString::from(key), bundle.as_os_str().to_owned());
    }
}

/// One-line stderr notice when TLS termination engages — the honesty bar (§5, option 2):
/// nub never silently decrypts, even when the user's own config demanded it.
fn emit_mitm_notice(policy: &SandboxPolicy) {
    let scope = if policy.net.brokers.is_empty() {
        "all allowed hosts (proxy: \"terminate\")".to_string()
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
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn set_proxy_env(command: &mut Command, port: u16, token: Option<&str>) {
    let url = match token {
        Some(t) => format!("http://{t}@127.0.0.1:{port}"),
        None => format!("http://127.0.0.1:{port}"),
    };
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
    ] {
        command.env(key, &url);
    }
    command.env("NODE_USE_ENV_PROXY", "1");
}

#[cfg(target_os = "linux")]
fn insert_proxy_env(env: &mut BTreeMap<OsString, OsString>, port: u16, token: Option<&str>) {
    let url = match token {
        Some(token) => format!("http://{token}@127.0.0.1:{port}"),
        None => format!("http://127.0.0.1:{port}"),
    };
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
    ] {
        env.insert(OsString::from(key), OsString::from(&url));
    }
    env.insert(OsString::from("NODE_USE_ENV_PROXY"), OsString::from("1"));
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
    apply_inner(policy, spec, None, None)
}

/// Apply with the verified runtime capability returned by [`earliest_bootstrap`].
/// Linux confinement requires this explicit embedder seam; other platforms accept
/// the value and retain their existing launch behavior.
pub fn apply_with_runtime(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    runtime: &RuntimeCapability,
) -> Result<Prepared, Degradation> {
    apply_inner(policy, spec, Some(runtime), None)
}

#[cfg(target_os = "linux")]
fn apply_with_retained_linux_authority(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    runtime: &RuntimeCapability,
    bwrap: std::fs::File,
) -> Result<Prepared, Degradation> {
    apply_inner(policy, spec, Some(runtime), Some(bwrap))
}

fn apply_inner(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    runtime: Option<&RuntimeCapability>,
    retained_bwrap: Option<std::fs::File>,
) -> Result<Prepared, Degradation> {
    #[cfg(not(target_os = "linux"))]
    let _ = (runtime, retained_bwrap);
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
    #[cfg(target_os = "linux")]
    let linux_preflight = linux::preflight(policy, &spec, runtime, retained_bwrap)?;
    // Start the per-host egress proxy FIRST (if the policy needs it), so its bound port
    // is threaded into the backend deny-layer (which permits egress ONLY to the proxy
    // endpoint) before the child is prepared. The proxy is then stashed on `Prepared`
    // so it outlives the child (design.md §2.5).
    let proxy = start_proxy_if_needed(policy);
    let proxy_port = proxy.as_ref().map(EgressProxy::port);
    // The per-session egress-proxy token, delivered to the child via the proxy URL. Same
    // presence as `proxy_port` (both derive from `proxy`), threaded into each backend so
    // the child authenticates to the loopback proxy.
    let proxy_token = proxy.as_ref().map(EgressProxy::token);
    // The child CA bundle, when TLS termination engaged. Linux receives the proxy's
    // sealed descriptor; other backends receive its ephemeral path.
    #[cfg(not(target_os = "linux"))]
    let ca_bundle = proxy.as_ref().and_then(|p| p.ca_bundle_path());
    #[cfg(target_os = "linux")]
    let ca_bundle = proxy
        .as_ref()
        .map(EgressProxy::ca_bundle_file)
        .transpose()
        .map_err(|error| Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(format!("cloning sealed CA bundle: {error}")),
        })?
        .flatten();
    let ca_bundle_present = ca_bundle.is_some();

    // Create the fresh per-run PRIVATE tmp dir up front (when the policy asks), so its
    // path is threaded into the backend BEFORE the child profile is built — the backend
    // grants it rw + points the child's TMPDIR at it + hides the shared system tmp. The
    // dir is owned by `Prepared` (moved in below) so it outlives the child and is removed
    // on drop. `None` for Shared/Deny.
    let private_tmp = make_private_tmp(policy);
    let tmp_dir = private_tmp.as_ref().map(|d| d.path());

    #[cfg(target_os = "macos")]
    let mut prepared = macos::apply(policy, spec, proxy_port, proxy_token, ca_bundle, tmp_dir)?;
    #[cfg(target_os = "linux")]
    let mut prepared = linux::apply(
        policy,
        spec,
        proxy_port,
        proxy_token,
        ca_bundle,
        tmp_dir,
        runtime,
        linux_preflight,
    )?;
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
    Ok(prepared)
}

fn validate_apply_inputs(policy: &SandboxPolicy, spec: &CommandSpec) -> Result<(), Degradation> {
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
#[cfg(not(target_os = "linux"))]
fn set_tmp_env(command: &mut Command, dir: &std::path::Path) {
    for key in ["TMPDIR", "TMP", "TEMP"] {
        command.env(key, dir);
    }
}

#[cfg(target_os = "linux")]
fn insert_tmp_env(env: &mut BTreeMap<OsString, OsString>, dir: &std::path::Path) {
    for key in ["TMPDIR", "TMP", "TEMP"] {
        env.insert(OsString::from(key), dir.as_os_str().to_owned());
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
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
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
        _private_tmp: None,
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
