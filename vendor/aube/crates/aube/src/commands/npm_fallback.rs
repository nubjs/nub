//! Stubs for npm-only commands that pnpm claims at the CLI surface.
//!
//! pnpm doesn't implement `whoami`, `token`, `owner`, `search`, `pkg`, or
//! `set-script` — it parses the command name, prints a "not implemented,
//! use npm" error, and exits non-zero. We match that behavior so these
//! names don't fall through aube's implicit-script runner (`External`),
//! where `aube whoami` would otherwise try to run a `whoami` script from
//! `package.json` and produce a confusing "script not found" error.

use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Args)]
pub struct FallbackArgs {
    /// Unused; captured so `aube <cmd> foo bar` parses instead of
    /// erroring on unexpected args before the fallback message prints.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub args: Vec<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub fn run(name: &str, args: &FallbackArgs) -> miette::Result<i32> {
    args.network.install_overrides();
    let cwd = crate::dirs::cwd()?;
    let files = crate::commands::FileSources::load(&cwd);
    let empty_ws = std::collections::BTreeMap::new();
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&empty_ws, env, &[]);

    if let Some(npm_path) = aube_settings::resolved::npm_path(&ctx) {
        let mut cmd = std::process::Command::new(&npm_path);
        cmd.arg(name)
            .args(&args.args)
            .stderr(aube_scripts::child_stderr());
        if let Some(registry) = args.network.registry.as_deref() {
            cmd.arg(format!("--registry={registry}"));
        }
        let status = cmd
            .status()
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to run configured npmPath `{npm_path}`"))?;
        return Ok(child_exit_code(status));
    }

    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_NPM_ONLY_COMMAND,
        "`aube {name}` is not implemented. This is an npm-only command — \
         run it with `npm {name}` instead, or set `npmPath` to let aube delegate it."
    ))
}

fn child_exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }

    1
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::child_exit_code;

    #[cfg(unix)]
    #[test]
    fn child_signal_exit_uses_shell_convention() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(2);

        assert_eq!(child_exit_code(status), 130);
    }

    #[cfg(unix)]
    #[test]
    fn child_normal_exit_code_is_preserved() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(7 << 8);

        assert_eq!(child_exit_code(status), 7);
    }
}
