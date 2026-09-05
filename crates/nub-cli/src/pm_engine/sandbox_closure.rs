//! Injects nub's shared zero-privilege sandbox engine (`nub-sandbox`) as the enforcement backend
//! for dependency lifecycle scripts, in place of aube's embedded Landlock/Seatbelt jail.
//!
//! aube-scripts owns the grant RESOLUTION — it turns the active `BuildPolicy` + project
//! `jailBuildPermissions` into a [`ScriptJail`](aube_scripts::ScriptJail) (package dir, extra
//! read/write paths, coarse network boolean) and hands it to the injected hook. This module maps
//! that resolved jail onto a `nub-sandbox` policy and confines the caller's own command through the
//! shared engine, so the build jail and the agent sandbox run ONE implementation (the epic 4.1
//! de-duplication) — and the build jail gains the shared engine's write-broker carve-outs.
//!
//! POSTURE — this reproduces today's aube jail exactly, so it cannot regress a working install:
//! read-anywhere, write only under {package dir, jail home, resolved write paths}, coarse egress
//! (all-or-nothing per the jail's `network`). Read-anywhere is a `"/"` read grant; `/dev`, the
//! system read floor and proc reads are added by the backend, not the surface. On Linux the
//! secret-deny floor the compiler adds is dropped by `enforce_pure_allowlist` (Landlock has no
//! deny), matching aube's read-`/` base; on macOS Seatbelt keeps it, a slightly stricter (secret-
//! protecting) read surface than aube's `(allow default)`. Tightening reads to a per-package
//! allowlist (the catalog's value) is a later, corpus-validated step, not this one.
//!
//! Linux (Landlock, `pre_exec`) and macOS (Seatbelt, `sandbox-exec` wrap) are wired. Windows keeps
//! aube's behavior — aube-scripts has no Windows jail today, so wiring AppContainer here is additive.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn register() {
    aube_scripts::set_script_sandbox(std::sync::Arc::new(confine));
}

/// No-op where the lifecycle seam is not wired (currently Windows), so aube's embedded jail — or
/// its no-op non-Linux/macOS arm — remains the enforcement path.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn register() {}

/// Linux: install nub's Landlock + seccomp confinement as a `pre_exec` on the caller's command.
/// The returned guard holds the Landlock ruleset descriptor open until the child is spawned.
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

/// macOS: rewrite the caller's command to `sandbox-exec -p <profile> -- <original command>`. There
/// is no descriptor to keep alive (Seatbelt reads the profile at spawn), so the guard is a unit.
#[cfg(target_os = "macos")]
fn confine(
    command: &mut tokio::process::Command,
    jail: &aube_scripts::ScriptJail,
    home: &std::path::Path,
) -> std::io::Result<Box<dyn Send>> {
    let policy = build_jail_policy(jail, home)
        .map_err(|err| std::io::Error::other(format!("build-jail policy compile failed: {err}")))?;
    // `None` means the policy needs no kernel wrap — leave the command as aube built it.
    let Some(profile) = nub_sandbox::build_jail_seatbelt_profile(&policy, None) else {
        return Ok(Box::new(()));
    };
    // aube has not yet applied cwd/env at hook time (run_script does that afterward, onto whatever
    // command it gets back), but `spawn_shell_with_settings` already set some env + kill_on_drop.
    // Carry the original program, args and env changes onto the `sandbox-exec` wrapper so the child
    // is unchanged; run_script's later cwd/env then apply to the wrapper and are inherited.
    let std_cmd = command.as_std();
    let program = std_cmd.get_program().to_os_string();
    let args: Vec<std::ffi::OsString> = std_cmd.get_args().map(|a| a.to_os_string()).collect();
    let envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = std_cmd
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(|v| v.to_os_string())))
        .collect();

    let mut wrapped = tokio::process::Command::new(nub_sandbox::SANDBOX_EXEC_PATH);
    wrapped
        .arg("-p")
        .arg(&profile)
        .arg("--")
        .arg(&program)
        .args(&args);
    for (key, value) in envs {
        match value {
            Some(value) => {
                wrapped.env(key, value);
            }
            None => {
                wrapped.env_remove(key);
            }
        }
    }
    wrapped.kill_on_drop(true);
    *command = wrapped;
    Ok(Box::new(()))
}

/// Build the `nub-sandbox` policy that reproduces aube's embedded jail posture for `jail`. The
/// surface is OS-agnostic; each backend lowers it (Landlock drops the secret denies, Seatbelt keeps
/// them) and adds its own device + system-read floor.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_jail_policy(
    jail: &aube_scripts::ScriptJail,
    home: &std::path::Path,
) -> Result<nub_sandbox::SandboxPolicy, nub_sandbox::CompileError> {
    use serde_json::json;

    let mut fs = serde_json::Map::new();
    // Read anywhere (aube's `add_rule(/, read_access)` / `(allow default)`); the write grants below
    // win under last-match-wins for their own subtrees. `/dev`, the system read floor and proc
    // reads are the backend's job — a surface allow under a reserved kernel tree is refused.
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
