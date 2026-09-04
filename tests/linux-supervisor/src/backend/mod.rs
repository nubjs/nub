// The module under test is the real file, pulled in verbatim so the harness compiles the
// exact committed code (no copy to drift). `pub(super)` inside it resolves against THIS
// `backend` module — the same relationship it has with `backend` inside nub-sandbox.
#[path = "../../../../crates/nub-sandbox/src/backend/linux_supervisor.rs"]
pub mod linux_supervisor;

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
