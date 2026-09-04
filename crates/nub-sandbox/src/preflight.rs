//! Why the sandbox cannot enforce on this host, as a CAUSE rather than a sentence.
//!
//! WHY THIS IS NOT STRING-MATCHING A FAILURE. The launcher's own diagnosis
//! (`backend::linux::classify_bwrap_failures`) reads a Bubblewrap failure string and turns it
//! into prose. That is the right tool AFTER a launch has failed, and the wrong one for deciding
//! what to tell a user to install: the wording varies by Bubblewrap version, and keying remedies
//! off it is exactly how a plain `EACCES` came to be reported as an AppArmor problem with a
//! remedy that could not help.
//!
//! So this asks the host directly — is a Bubblewrap present at all, is the kernel restricting
//! unprivileged user namespaces, is nub's helper installed, can this session execute it — and
//! returns the answer as an enum. The caller renders it; nothing here writes copy, so the
//! refusal message and the readiness report cannot disagree about the cause.
//!
//! It is deliberately CHEAP and side-effect-free: a few `stat`s and one small file read, no
//! launches. It runs on the failure path of an install, where a slow probe would be its own
//! problem.

/// The elevated command that prepares a Linux host, and the group its default install gates the
/// helper behind. Both live HERE, beside the causes they remedy, rather than in the Linux
/// backend: the code that has to NAME them is the install-refusal copy, which compiles on every
/// platform while `backend::linux_setup` compiles on one. The Linux backend re-exports these
/// rather than restating them, so there is still exactly one spelling of each.
pub const LINUX_SETUP_COMMAND: &str = "sudo nub setup-sandbox";
pub const LINUX_HELPER_GROUP: &str = "nub-sandbox";

/// A prerequisite the host does not meet. Each variant is a DIFFERENT remedy — collapsing any
/// two of them is how a user ends up running a command that cannot fix their machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// No Bubblewrap anywhere the resolver looks. The remedy is the distro package; no nub
    /// command installs one.
    Bubblewrap,
    /// A Bubblewrap exists, but the kernel refuses unprivileged user namespaces to it. The
    /// remedy is nub's one-time setup, which installs a path-keyed AppArmor grant.
    NamespacePermission,
    /// The helper and its profile are installed and working. The caller's SESSION cannot
    /// execute the helper, because its group set was fixed before the group existed. No
    /// re-run of setup changes that; only a fresh login or `sg` does.
    SessionGroup,
    /// Seatbelt's stock entry point is absent. Not repairable by any nub command.
    SeatbeltUnavailable,
}

/// What the host is missing, or `None` when the sandbox can enforce.
///
/// A `None` is not a promise the next launch succeeds — only a real launch proves that. It
/// means no prerequisite is KNOWN to be missing, which is the honest precondition for letting
/// the launcher's own diagnosis speak instead.
pub fn diagnose() -> Option<Missing> {
    platform::diagnose()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Missing;

    /// STUB (epic row 0.3). The former diagnosis probed the privileged Bubblewrap + helper-account
    /// tier (`linux_setup::userns_restricted` / `helper_readiness`), which was dropped with the
    /// curated zero-privilege import. The zero-privilege tier (Landlock + a seccomp user-notify
    /// supervisor) has its own, different prerequisites — an unprivileged Landlock ABI probe rather
    /// than a namespace/AppArmor grant — so this reports no known missing prerequisite until that
    /// tier lands, and the launcher's own failure diagnosis speaks. See epics/sandbox/design.md.
    pub(super) fn diagnose() -> Option<Missing> {
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Missing;

    /// Whether the stock Seatbelt entry point is present and executable. Inlined here with the
    /// curated zero-privilege import (epic row 0.3), which dropped `backend::macos_setup` — the
    /// no-op macOS half of `nub setup-sandbox` that formerly owned this probe. The one fact it
    /// answers (can Seatbelt confine on this host) lives beside the cause it reports.
    fn seatbelt_enforceable() -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(crate::backend::SANDBOX_EXEC_PATH)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }

    pub(super) fn diagnose() -> Option<Missing> {
        if seatbelt_enforceable() {
            return None;
        }
        Some(Missing::SeatbeltUnavailable)
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::Missing;

    /// The build jail is the AppContainer path on Windows, which needs no provisioning — the
    /// dedicated account exists for the agent-sandbox grammar, not for this. So there is no
    /// host prerequisite to report, and a fail-closed install here is a POLICY problem rather
    /// than a missing-package one; the launcher's own reason is the right thing to surface.
    pub(super) fn diagnose() -> Option<Missing> {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::Missing;

    pub(super) fn diagnose() -> Option<Missing> {
        None
    }
}
