//! Host-setup reporting for the macOS agent-sandbox — `nub setup-sandbox`.
//!
//! THERE IS NOTHING TO INSTALL, AND THAT IS THE WHOLE DESIGN. Seatbelt is a kernel primitive any
//! process may ask for, reached through `/usr/bin/sandbox-exec`, which ships with macOS and needs
//! no privilege — so macOS pays none of the one-time root/administrator cost Linux and Windows
//! do. `setup` and `teardown` therefore have real work to do on neither, and the only question
//! left for `status` is whether that stock binary is where the launcher expects it.
//!
//! That question is not rhetorical: `sandbox-exec` is deprecated by Apple, and a host whose
//! system volume has been altered can lack it. Reporting its absence is the difference between a
//! sandbox that fails at launch for an unexplained reason and one that says so up front.

use super::StatusReport;

/// The stock entry point to Seatbelt, re-exported from the launcher that spawns it so the
/// readiness probe and the thing it predicts cannot drift.
pub use super::macos::SANDBOX_EXEC_PATH;

/// Whether the sandbox can confine a command on this host right now.
pub fn enforceable() -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(SANDBOX_EXEC_PATH)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// UNPRIVILEGED. Report whether the sandbox can enforce, changing nothing.
pub fn status() -> StatusReport {
    let ready = enforceable();
    let text = if ready {
        format!(
            "seatbelt: available at {SANDBOX_EXEC_PATH}\nhost setup: not required on macOS\n\n\
             The sandbox can enforce on this host.\n"
        )
    } else {
        format!(
            "seatbelt: NOT available ({SANDBOX_EXEC_PATH} is missing or not executable)\n\
             host setup: not required on macOS\n\nThe sandbox cannot enforce on this host. Nub \
             confines a command through the stock {SANDBOX_EXEC_PATH}; restore it from a stock \
             macOS system volume.\n"
        )
    };
    StatusReport { text, ready }
}

/// UNPRIVILEGED, and a deliberate no-op. Reported rather than silently succeeding, so a reader
/// who ran the Linux or Windows setup and then this one learns why nothing happened.
pub fn setup() -> String {
    if enforceable() {
        return format!(
            "Nothing to set up. Seatbelt is unprivileged on macOS, so the sandbox needs no \
             root-owned helper and no dedicated account — it confines a command through the stock \
             {SANDBOX_EXEC_PATH} and is ready now.\n"
        );
    }
    format!(
        "Nothing to set up. Seatbelt is unprivileged on macOS, so the sandbox needs no \
         installation — but it reaches the kernel through the stock {SANDBOX_EXEC_PATH}, which \
         is missing or not executable here. Restore it from a stock macOS system volume.\n"
    )
}

/// UNPRIVILEGED, and a deliberate no-op — [`setup`] installed nothing to remove.
pub fn teardown() -> String {
    "Nothing to remove. The macOS sandbox installs nothing on the host.\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stock_macos_host_reports_it_can_enforce() {
        // The probe is the whole of macOS readiness, and every test host is a stock macOS host,
        // so a false here means the probe — not the host — is wrong.
        assert!(
            enforceable(),
            "{SANDBOX_EXEC_PATH} is missing or not executable on a stock macOS host"
        );
        let report = status();
        assert!(report.ready, "{}", report.text);
        assert!(report.text.contains("can enforce"), "{}", report.text);
    }
}
