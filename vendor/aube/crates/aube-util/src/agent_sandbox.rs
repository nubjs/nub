//! Detection of a coding agent's command sandbox around this process.
//!
//! Codex CLI and Claude Code run shell commands inside deny-by-default OS
//! sandboxes (Seatbelt on macOS, Landlock/seccomp or bubblewrap on Linux):
//! writes are confined to the workspace and temp dirs, and network is either
//! off or allowlisted through a proxy. Both announce themselves to the child
//! through environment variables, and that is the whole signal used here —
//! nothing is probed. Two behaviors key off it: a denied connection is a
//! hard fact inside such a sandbox, not a transient to retry, and error help
//! should name the sandbox instead of suggesting registry credentials.
//!
//! Sources: `codex-rs/core/src/spawn.rs` (`CODEX_SANDBOX`,
//! `CODEX_SANDBOX_NETWORK_DISABLED`) and sandbox-runtime
//! `src/sandbox/sandbox-utils.ts` (`SANDBOX_RUNTIME`).

use std::error::Error;
use std::io::ErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSandbox {
    Codex,
    ClaudeCode,
}

impl AgentSandbox {
    /// The product name as a user would write it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::ClaudeCode => "Claude Code",
        }
    }
}

fn env_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

/// The agent sandbox this process runs under, if any.
pub fn detect() -> Option<AgentSandbox> {
    if env_set("CODEX_SANDBOX") {
        Some(AgentSandbox::Codex)
    } else if env_set("SANDBOX_RUNTIME") {
        Some(AgentSandbox::ClaudeCode)
    } else {
        None
    }
}

/// The sandbox has told us outright that there is no network. Only Codex
/// signals this; Claude Code's runtime proxies per domain and says nothing.
pub fn network_disabled() -> bool {
    std::env::var_os("CODEX_SANDBOX_NETWORK_DISABLED").is_some_and(|v| v == "1")
}

/// Help for a request the registry client classified as a network deny.
/// Names the agent sandbox when one is detected — "check auth" is advice an
/// agent may act on by poking at credentials — and outside one still blames
/// policy (a firewall, a network namespace), never the registry.
pub fn network_denied_help() -> String {
    network_denied_help_for(detect())
}

/// [`network_denied_help`] with the sandbox injected, for callers that pin it
/// in tests without touching the environment.
pub fn network_denied_help_for(sandbox: Option<AgentSandbox>) -> String {
    match sandbox {
        Some(sandbox) => format!(
            "network access is blocked by the {} sandbox — rerun the command outside the sandbox, or use `--offline` to install from what the store already holds",
            sandbox.label()
        ),
        None => "the connection was refused by policy, not by the registry — a sandbox, firewall, or network namespace is blocking it; allow the registry there, or use `--offline` to install from what the store already holds".to_string(),
    }
}

/// Whether a transport error is a hard network deny that no retry can fix.
///
/// Always hard: the sandbox said network is off, or the OS refused the socket
/// outright (`EPERM`/`EACCES` from Seatbelt or seccomp, `ENETUNREACH` /
/// `EHOSTUNREACH` from an empty network namespace). Hard only inside a
/// detected agent sandbox: a refused connection or a refused proxy tunnel,
/// which is how an allowlisting proxy says no — outside a sandbox those can
/// be a registry mirror mid-restart and stay retriable.
pub fn is_hard_network_deny(err: &(dyn Error + 'static)) -> bool {
    if network_disabled() {
        return true;
    }
    let sandboxed = detect().is_some();
    let mut cur: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            match io.kind() {
                ErrorKind::PermissionDenied
                | ErrorKind::NetworkUnreachable
                | ErrorKind::HostUnreachable => return true,
                ErrorKind::ConnectionRefused if sandboxed => return true,
                _ => {}
            }
        }
        if sandboxed && e.to_string().contains("tunnel") {
            return true;
        }
        cur = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(kind: ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "probe")
    }

    #[derive(Debug)]
    struct Wrapped(std::io::Error);
    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error sending request")
        }
    }
    impl Error for Wrapped {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    // Env-free classification only: the env-dependent branches read process
    // globals that would race sibling tests.
    #[test]
    fn os_level_denies_are_hard_through_the_source_chain() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NetworkUnreachable,
            ErrorKind::HostUnreachable,
        ] {
            assert!(is_hard_network_deny(&Wrapped(io(kind))), "{kind:?}");
        }
    }

    #[test]
    fn refused_and_timeouts_stay_retriable_outside_a_sandbox() {
        if detect().is_some() || network_disabled() {
            return; // the test process itself is sandboxed; the premise does not hold
        }
        for kind in [ErrorKind::ConnectionRefused, ErrorKind::TimedOut] {
            assert!(!is_hard_network_deny(&Wrapped(io(kind))), "{kind:?}");
        }
    }
}
