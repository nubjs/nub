use clap::Args;
use miette::IntoDiagnostic;
use miette::miette;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct NodeArgs {
    /// Arguments to pass to Node.js
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

pub async fn run(args: NodeArgs) -> miette::Result<Option<i32>> {
    crate::runtime::ensure_for_cwd(&crate::dirs::cwd()?).await?;
    let node = crate::runtime::node_program();

    #[cfg(unix)]
    let mut cmd = std::process::Command::new(&node);
    #[cfg(not(unix))]
    let mut cmd = tokio::process::Command::new(&node);

    cmd.args(&args.args);
    let runtime_dirs = crate::runtime::path_entries();
    if !runtime_dirs.is_empty() {
        cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
    }
    // `NODE` / `npm_node_execpath` and any embedder env (e.g. a wrapper's
    // `NODE_OPTIONS` preload), so a direct `aube node` is invoked the same
    // way lifecycle scripts are.
    for (key, value) in crate::runtime::child_env_vars() {
        cmd.env(key, value);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_EXEC_FAILED,
            "failed to exec node at {}: {err}",
            node.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().await.into_diagnostic().map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_SHIM_EXEC_FAILED,
                "failed to spawn node at {}: {e}",
                node.display()
            )
        })?;
        Ok(status.code())
    }
}

/// Run Node as a supervised child (spawn + wait) rooted at an explicit
/// `base_dir` instead of the process cwd — the in-process embedding
/// entry. Unlike the CLI [`run`], this never image-replaces the process,
/// so a host can call it in-process and get the child's exit code back.
/// `base_dir` roots runtime resolution; `None` uses the process cwd.
pub async fn run_spawn(
    args: Vec<OsString>,
    base_dir: Option<PathBuf>,
) -> miette::Result<Option<i32>> {
    let cwd = match base_dir {
        Some(dir) => dir,
        None => crate::dirs::cwd()?,
    };
    crate::runtime::ensure_for_cwd(&cwd).await?;
    let node = crate::runtime::node_program();
    let mut cmd = tokio::process::Command::new(&node);
    cmd.args(&args);
    cmd.current_dir(&cwd);
    let runtime_dirs = crate::runtime::path_entries();
    if !runtime_dirs.is_empty() {
        cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
    }
    for (key, value) in crate::runtime::child_env_vars() {
        cmd.env(key, value);
    }
    // Tie the child to aube's lifetime (signal forwarding + PDEATHSIG) the
    // same way dlx/exec supervised children are, so a host that stops the
    // embedding process doesn't strand Node. Drop-in for `cmd.status()`.
    let status = crate::process_guard::spawn_and_wait(cmd)
        .await
        .into_diagnostic()
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_SHIM_EXEC_FAILED,
                "failed to spawn node at {}: {e}",
                node.display()
            )
        })?;
    Ok(status.code())
}
