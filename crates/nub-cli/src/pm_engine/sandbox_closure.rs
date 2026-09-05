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
//! POSTURE — reproduces aube's jail closely, so it does not regress a working install: reads are the
//! whole disk MINUS secret subtrees, write only under {package dir, jail home, resolved write paths},
//! coarse egress (all-or-nothing per the jail's `network`). The reads are NOT a surface `"/"` grant —
//! the Landlock lowering silently drops a whole-root read as an unclawable credential leak — but a
//! post-compile `relax_reads_to_disk_minus_secrets` relaxation; `/dev`, the system read floor and proc
//! reads are added by the backend, not the surface. This is slightly STRICTER than aube's read-`/` /
//! `(allow default)`: `$HOME`-anchored secret subtrees are excluded (a `.env*` basename residual
//! remains on Landlock, which has no deny). Tightening reads further to a per-package allowlist (the
//! catalog's value) is a later, corpus-validated step, not this one.
//!
//! Linux (Landlock, `pre_exec`) and macOS (Seatbelt, `sandbox-exec` wrap) are wired. Windows keeps
//! aube's behavior — aube-scripts has no Windows jail today, so wiring AppContainer here is additive.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn register() {
    aube_scripts::set_script_sandbox(std::sync::Arc::new(confine));
}

/// Windows: the aube-scripts lifecycle seam is not wired here yet (aube's no-op non-Linux/macOS
/// arm stays the build-jail path), but the `nub-sandbox` AppContainer backend still needs to know
/// HOW to launch nub as the co-package egress-proxy HELPER for a zero-privilege per-host net policy
/// (the agent sandbox reaches this independently of the build jail). Register that launch command —
/// `[current_exe(), "<hidden-flag>"]`; the backend appends the per-run serialized policy, and the
/// hidden re-entry is dispatched in `cli::run` to `nub_sandbox::serve_windows_egress_helper`.
#[cfg(target_os = "windows")]
pub(crate) fn register() {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("nub"));
    nub_sandbox::set_windows_egress_helper_command(vec![
        exe.into_os_string(),
        std::ffi::OsString::from(crate::cli::EGRESS_FUNNEL_HELPER_FLAG),
    ]);
}

/// No-op where neither seam is wired (non-Linux/macOS/Windows), so aube's embedded jail — or its
/// no-op arm — remains the enforcement path.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
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
    // Only the WRITE grants go on the surface. Reads are relaxed to disk-minus-secrets AFTER compile
    // (below) rather than named here. A surface `"/": "r"` grant does NOT work: the Landlock lowering
    // (`nub-sandbox backend::linux_grants::compile_mount_plan`) deliberately DROPS a whole-root read
    // as an unclawable credential leak, so read-`/` silently collapsed to system-floor reads — which
    // broke every `node`-spawning lifecycle script, since node could not read nub's injected preload.
    // Writes stay allow-only and win under last-match-wins over the broad read allows. `/dev`, the
    // system read floor and proc reads are the backend's job.
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
    let mut policy = nub_sandbox::compile(&surface, &ctx)?;
    // Generous reads (disk minus secret subtrees), the sandbox-sanctioned "read almost everything":
    // reproduces aube's read-anywhere posture minus `$HOME`-anchored secrets, so lifecycle tooling
    // (node + its runtime/preload, module tree, system libs) reads what it needs while writes stay
    // confined. On Linux, Landlock drops the secret denies but the allows already exclude the secret
    // subtrees; Seatbelt keeps them. `.env*` basename reads are a per-backend residual.
    //
    // ⛔ Anchor the secret exclusion to the REAL user home, NOT the throwaway jail home (`home`). The
    // jail home is an empty per-package dir, so `disk_minus_secrets_read_allows` anchored to it would
    // exclude nothing real and still grant read to the user's actual `~/.ssh` — measured leaking under
    // the 5.2c adversarial sweep. `cache` follows the real home for the same reason; `tmp` and
    // `project` stay the jail's (they carry no user secrets and the write grants already cover them).
    let real_home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.to_path_buf());
    let secret_homes = nub_sandbox::Homes {
        cache: real_home.join(".cache"),
        home: real_home,
        tmp: home.join("tmp"),
        project: jail.package_dir.clone(),
    };
    nub_sandbox::relax_reads_to_disk_minus_secrets(&mut policy, &secret_homes);
    Ok(policy)
}
