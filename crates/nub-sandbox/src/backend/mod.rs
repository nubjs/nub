//! Per-OS enforcement backends and the [`apply`] entry that turns a resolved
//! [`SandboxPolicy`] into a launch-ready child.
//!
//! The enforcement contract is FAIL-SAFE-WITH-DEGRADATION, not fail-open (ported
//! from the reviewed salvage `backend/mod.rs`): a backend NEVER silently drops an
//! axis it claimed to enforce. When a primitive is unavailable it records the
//! loss in [`Degradation`] so the caller surfaces a WARNING; a hard fail-closed
//! (a required axis unenforceable) is `Err(Degradation)`.
//!
//! BACKEND STATUS: macOS (Seatbelt, [`macos`]), Linux (Landlock+seccomp,
//! [`linux`]), and Windows (AppContainer LowBox, [`windows`]) are wired; any other
//! OS runs the env-scrub-only [`generic_apply`] skeleton — which constructs the
//! child env and reports fs/net as NOT enforced. Every path preserves the API shape
//! (`apply(policy, spec) -> Result<Prepared, Degradation>`) the future embedder
//! seam slots into.
//!
//! LAUNCH SEAM: a backend either configures [`Prepared::command`] for the caller to
//! spawn (macOS wraps `sandbox-exec`; Linux installs a `pre_exec` hook; the skeleton
//! just scrubs env) OR — Windows only — OWNS the whole spawn lifecycle, because
//! neither Windows launch can be a pre-built `std::process::Command`: the AppContainer
//! one needs a custom `CreateProcessW` with `STARTUPINFOEX`/`SECURITY_CAPABILITIES`,
//! the dedicated-account one needs `CreateProcessWithLogonW`, and BOTH need per-run
//! ACL grants TORN DOWN after the child exits. [`Prepared::status`] is the uniform
//! verb: mac/linux/skeleton delegate to `command.status()`; Windows runs whichever
//! launcher its plan names (setup → spawn → wait → RAII teardown).

use crate::policy::{Effect, Inspection, ProxyMode, SandboxPolicy};
use crate::proxy::mitm::MitmEngine;
use crate::proxy::{EgressProxy, StaticDecider};
use std::process::Command;
use std::sync::Arc;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

// The unprivileged seccomp USER_NOTIF supervisor that closes the port-scoped
// ConnectTcp residual in NetMode::Proxy (Linux-only mechanism; see the module doc).
#[cfg(target_os = "linux")]
mod linux_connect_notify;

// The Windows AppContainer backend. Compiled on Windows (its real consumer) and
// under `test` on any host — so its OS-agnostic IR→plan derivation (grant carve,
// capability selection, dangerous-root guard) is unit-tested on the macOS dev host
// without a Windows machine (the FFI launcher itself stays `#[cfg(windows)]`).
#[cfg(any(target_os = "windows", test))]
mod windows;

// The Windows dedicated-account + WFP backend (agent-sandbox). Same cfg as `windows` so its
// OS-agnostic plan derivation and mode-selection are unit-tested on the macOS dev host.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod windows_account;

// The OS-agnostic Landlock grant derivation. Compiled on Linux (its real consumer)
// and under `test` on any host — so its security-critical carve logic is unit-tested
// on the macOS dev host over tempfile trees, without a kernel.
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
}

impl CommandSpec {
    pub fn new(program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
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
}

/// A launch-ready child: a configured [`Command`] plus the [`Degradation`] the
/// backend achieved. The caller surfaces `degradation`, then launches with
/// [`Prepared::status`] (NOT `command.status()` directly — Windows enforcement
/// rides the `status` seam, not the `command` field).
pub struct Prepared {
    /// The configured child for the mac/linux/skeleton path. On Windows this is the
    /// env-scrubbed plain child used ONLY when no confinement mechanism applies
    /// (`launch` is `None`); when one does — either Windows variant — `launch` owns the
    /// spawn and this field is unused.
    pub command: Command,
    pub degradation: Degradation,
    /// The running egress proxy (design.md §2.5), when the policy enforces per-host
    /// net. It runs in the nub PARENT and MUST outlive the child, so it is owned here:
    /// [`Prepared::status`] holds it for the child's whole run, and dropping this
    /// value stops the listener. `None` when net is unconfined or coarse-deny (no
    /// proxy needed). Set by [`apply`], not the per-OS backends.
    pub(crate) proxy: Option<EgressProxy>,
    /// Linux per-host connect-notify supervisor (closes the ConnectTcp port residual).
    /// When `Some`, [`Prepared::status`] spawns `command`, receives the child's seccomp
    /// listener fd, and runs the supervisor for the child's lifetime instead of a plain
    /// `command.status()`. Set only by [`linux::apply`] in `NetMode::Proxy` (and only
    /// where the supervisor is viable); `None` otherwise.
    #[cfg(target_os = "linux")]
    pub(crate) connect_notify: Option<linux_connect_notify::ConnectNotify>,
    /// Windows launch plan — the backend owns spawn+wait+teardown when this is `Some`.
    /// A two-variant enum, one per Windows mechanism: the per-run AppContainer (LowBox
    /// token) and the dedicated local account (`CreateProcessWithLogonW`, no LowBox
    /// token). Absent (or on other OSes) → [`Prepared::status`] spawns `command`.
    #[cfg(target_os = "windows")]
    pub(crate) launch: Option<windows::WindowsLaunch>,
}

impl Prepared {
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
    pub fn status(mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(target_os = "windows")]
        if let Some(launch) = self.launch.take() {
            return launch.run();
        }
        // Linux per-host: run the connect-notify supervisor over the child's lifetime.
        // `self` (holding `self.proxy`) outlives this call, so the egress proxy stays up.
        #[cfg(target_os = "linux")]
        if let Some(cn) = self.connect_notify.take() {
            return cn.run(&mut self.command);
        }
        self.command.status()
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
    EgressProxy::start_in_range(decider, mitm, proxy_port_range(policy)).ok()
}

/// The loopback window the proxy must bind inside, when one applies.
///
/// Only Windows' dedicated-account backend has one: its WFP permit is installed ONCE, by the
/// elevated setup, over a fixed port window — so the proxy binds into that window rather than
/// a filter chasing an ephemeral port, which would cost a UAC prompt every run. Everywhere
/// else the deny-layer carves the exact port at launch and no range is needed.
#[cfg(target_os = "windows")]
fn proxy_port_range(policy: &SandboxPolicy) -> Option<(u16, u16)> {
    if !windows_account::needs_account_backend(policy) {
        return None;
    }
    // An absent or unreadable marker is NOT decided here: `windows_account::apply` fails
    // closed with the actionable "run the elevated setup" message, and an ephemeral bind in
    // the meantime is harmless because that launch never happens.
    let m = windows_account::state::read_marker().ok().flatten()?;
    Some((m.port_low, m.port_high))
}

#[cfg(not(target_os = "windows"))]
fn proxy_port_range(_policy: &SandboxPolicy) -> Option<(u16, u16)> {
    None
}

/// The CA-trust env keys pointed at the child CA bundle (ephemeral CA + real roots).
/// A union of the common tool conventions — `NODE_EXTRA_CA_CERTS` is ADDITIVE (Node
/// keeps its built-in roots); the rest REPLACE the store, which is exactly why the bundle
/// carries the real roots alongside the CA. Brand-clean: every key is a tool's own
/// documented convention, none nub's. Set AFTER `env_clear` so it survives the scrub.
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

/// Apply a resolved policy to a command, dispatching to the per-OS backend.
///
/// The env axis is enforced by CONSTRUCTION (not an OS primitive): when the policy
/// enforces env, the child env is cleared and set to exactly the policy's
/// constructed map. Each OS backend additionally hardens a scrubbed env so the
/// withheld secret can't be recovered from a co-resident same-uid process: Linux via
/// seccomp (`ptrace`/`process_vm_readv` deny — memory scrape) + Landlock (`/proc`
/// close — the `/proc/<ppid>/environ` file); macOS via the Seatbelt env-read closure
/// (`deny process-info*` + self-restore, which shuts the `KERN_PROCARGS2` argv/env
/// read). Because the macOS closure lives in the SBPL profile, a policy that withholds
/// a secret is wrapped even when fs/net are relaxed (see `macos::needs_wrap`). fs/net
/// enforcement is the backend's job; on an OS whose backend has not landed,
/// [`generic_apply`] reports them as not-enforced (never silent).
pub fn apply(policy: &SandboxPolicy, spec: CommandSpec) -> Result<Prepared, Degradation> {
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
    // The child CA-bundle path, when TLS termination engaged. Threaded into each backend
    // so the child both TRUSTS the minted leaves (CA-env) and can READ the bundle even
    // under a confining fs policy (an explicit read grant — nub infra, not user config).
    let ca_bundle = proxy.as_ref().and_then(|p| p.ca_bundle_path());

    #[cfg(target_os = "macos")]
    let mut prepared = macos::apply(policy, spec, proxy_port, proxy_token, ca_bundle)?;
    #[cfg(target_os = "linux")]
    let mut prepared = linux::apply(policy, spec, proxy_port, proxy_token, ca_bundle)?;
    #[cfg(target_os = "windows")]
    let mut prepared = windows::apply(policy, spec, proxy_port, proxy_token, ca_bundle)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let mut prepared = generic_apply(policy, spec, proxy_port, proxy_token, ca_bundle)?;

    // One-line stderr notice when TLS termination ACTUALLY engages — never silent, but
    // never MISLEADING either: suppress it where the backend degraded net (e.g. Windows,
    // whose AppContainer child can't reach the loopback proxy, so termination never
    // happens and the request is fail-safe denied — announcing it would be a false claim).
    if ca_bundle.is_some()
        && !prepared
            .degradation
            .lost
            .iter()
            .any(|l| l.starts_with("net-per"))
    {
        emit_mitm_notice(policy);
    }

    prepared.proxy = proxy;
    Ok(prepared)
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
) -> Result<Prepared, Degradation> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }

    // Env axis — construction, not interception.
    if policy.env.enforce {
        command.env_clear();
        for (k, v) in &policy.env.constructed {
            command.env(k, v);
        }
    }
    if let Some(port) = proxy_port {
        set_proxy_env(&mut command, port, proxy_token);
    }
    if let Some(bundle) = ca_bundle {
        set_ca_env(&mut command, bundle);
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
    })
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
