//! Injects nub's shared zero-privilege sandbox engine (`nub-sandbox`) as the enforcement backend
//! for dependency lifecycle scripts, in place of aube's embedded Landlock jail.
//!
//! aube-scripts owns the grant RESOLUTION — it turns the active `BuildPolicy` + project
//! `jailBuildPermissions` into a [`ScriptJail`](aube_scripts::ScriptJail) (package dir, extra
//! read/write paths, coarse network boolean) and hands it to the injected hook. This module maps
//! that resolved jail onto a `nub-sandbox` policy and confines the caller's own command through the
//! shared engine, so the build jail and the agent sandbox run ONE implementation (the epic 4.1
//! de-duplication) — and the build jail gains the shared engine's write-broker carve-outs.
//!
//! POSTURE — this reproduces today's aube jail exactly, so it cannot regress a working install:
//! read-anywhere, write only under {package dir, jail home, `/dev`, resolved write paths}, coarse
//! egress (all-or-nothing per the jail's `network`). Read-anywhere is expressed as a `"/"` read
//! grant; the compiler's secret-deny floor nests inside it but Landlock has no deny primitive, so
//! `enforce_pure_allowlist` drops it — matching aube's `add_rule(/, read)` base. Tightening the
//! read surface to a per-package allowlist (the catalog's value) is a later, corpus-validated step,
//! not this one.
//!
//! Linux-only for now: the macOS Seatbelt and Windows AppContainer lifecycle seams are still being
//! wired, so on those platforms no hook is installed and aube's embedded jail keeps running.

#[cfg(target_os = "linux")]
pub(crate) fn register() {
    aube_scripts::set_script_sandbox(std::sync::Arc::new(confine));
}

/// No-op off Linux — the platform lifecycle seams are not wired yet, so aube's embedded jail
/// remains the enforcement path there.
#[cfg(not(target_os = "linux"))]
pub(crate) fn register() {}

#[cfg(target_os = "linux")]
fn confine(
    command: &mut tokio::process::Command,
    jail: &aube_scripts::ScriptJail,
    home: &std::path::Path,
) -> std::io::Result<Box<dyn Send>> {
    let policy = build_jail_policy(jail, home)
        .map_err(|err| std::io::Error::other(format!("build-jail policy compile failed: {err}")))?;
    // `entry_program` / `tmp_dir` are None: read-anywhere already covers the interpreter, and aube
    // points TMPDIR at the (write-granted) jail home rather than a private per-run dir.
    //
    // `as_std_mut`: `tokio::process::Command` does NOT implement the std `CommandExt` trait
    // (it re-exposes `pre_exec` as an inherent method), but its inner `std::process::Command` does,
    // and tokio's spawn honors a `pre_exec` installed on that inner command — so the confinement
    // runs between fork and exec exactly as on a plain std command.
    let guard = nub_sandbox::confine_build_jail_command(command.as_std_mut(), &policy, None, None)
        .map_err(|degradation| {
            std::io::Error::other(format!(
                "build-jail confinement lost {:?}: {}",
                degradation.lost,
                degradation.reason.as_deref().unwrap_or("no detail")
            ))
        })?;
    Ok(Box::new(guard))
}

/// Build the `nub-sandbox` policy that reproduces aube's embedded jail posture for `jail`.
#[cfg(target_os = "linux")]
fn build_jail_policy(
    jail: &aube_scripts::ScriptJail,
    home: &std::path::Path,
) -> Result<nub_sandbox::SandboxPolicy, nub_sandbox::CompileError> {
    use serde_json::json;

    let mut fs = serde_json::Map::new();
    // Read anywhere (aube's `add_rule(/, read_access)`); the write grants below win under
    // last-match-wins for their own subtrees. `/dev` is NOT granted here — the Landlock backend
    // adds the device tree itself (a surface allow under the reserved `/dev` kernel tree is
    // refused), and it also adds its own system read floor + proc reads.
    fs.insert("/".to_string(), json!("r"));
    fs.insert(home.to_string_lossy().into_owned(), json!("rw"));
    fs.insert(jail.package_dir.to_string_lossy().into_owned(), json!("rw"));
    for path in &jail.write_paths {
        fs.insert(path.to_string_lossy().into_owned(), json!("rw"));
    }
    let surface = json!({ "fs": fs, "net": jail.network });

    let homes = nub_sandbox::Homes {
        home: home.to_path_buf(),
        tmp: home.join("tmp"),
        cache: home.join(".cache"),
        project: jail.package_dir.clone(),
    };
    let ctx = nub_sandbox::CompileCtx::new(
        homes,
        jail.package_dir.clone(),
        nub_sandbox::ScopeCapabilities::approved(),
        std::env::vars().collect(),
    );
    nub_sandbox::compile(&surface, &ctx)
}
