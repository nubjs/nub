// The module under test is the real file, pulled in verbatim so the harness compiles the
// exact committed code (no copy to drift). `pub(super)` inside it resolves against THIS
// `backend` module — the same relationship it has with `backend` inside nub-sandbox.
#[path = "../../../../crates/nub-sandbox/src/backend/linux_supervisor.rs"]
pub mod linux_supervisor;

// `spawn_supervised` (the library launch path in the module under test) references
// `super::linux_landlock::{drop_all_capabilities, restrict_self}`. The harness exercises the
// STANDALONE supervisor (`run_supervised`) and never `spawn_supervised`, so these are compile-only
// stubs — present solely so the module under test resolves. Signatures match the real
// `linux_landlock` so the reference type-checks exactly as it does in nub-sandbox.
#[allow(dead_code)]
pub mod linux_landlock {
    use std::os::fd::RawFd;
    pub(super) unsafe fn drop_all_capabilities() -> std::io::Result<()> {
        Ok(())
    }
    pub(crate) unsafe fn restrict_self(_ruleset_fd: RawFd) -> Result<(), libc::c_int> {
        Ok(())
    }
}

// Replicates the EXACT call shapes from crates/nub-sandbox/src/backend/linux_landlock.rs
// (lines 787 and 795-797), with `super::linux_monitor::` rewritten to the sibling module
// under test. If the ported signatures did not match the call sites, this fails to compile,
// so it is a compile-time proof that the two helpers match their consumers.
#[allow(dead_code)]
fn _landlock_call_site_signature_check(
    seccomp: Option<std::sync::Arc<Vec<seccompiler::sock_filter>>>,
) -> std::io::Result<()> {
    linux_supervisor::mark_inherited_fds_cloexec()?;
    if let Some(filter) = &seccomp {
        linux_supervisor::install_target_seccomp(filter)
            .map_err(std::io::Error::from_raw_os_error)?;
    }
    Ok(())
}
