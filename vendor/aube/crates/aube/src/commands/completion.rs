use miette::{IntoDiagnostic, Result, WrapErr};
use std::process::Command;

#[derive(clap::Args)]
pub struct CompletionArgs {
    /// The shell to generate completions for (bash, zsh, fish)
    #[arg(value_name = "SHELL")]
    pub shell: String,
}

pub async fn run(args: CompletionArgs) -> Result<()> {
    // Command name from the active embedder (standalone aube → "aube").
    let name = aube_util::embedder().name;
    let usage_cmd = format!("{name} usage");
    let output = Command::new("usage")
        .args([
            "g",
            "completion",
            &args.shell,
            name,
            "--usage-cmd",
            &usage_cmd,
            "--cache-key",
            env!("CARGO_PKG_VERSION"),
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to invoke `usage`; install it from https://usage.jdx.dev")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(miette::miette!(
            code = aube_codes::errors::ERR_AUBE_COMPLETION_FAILED,
            "`usage g completion {}` failed: {}",
            args.shell,
            stderr.trim()
        ));
    }

    std::io::Write::write_all(&mut std::io::stdout(), &output.stdout)
        .into_diagnostic()
        .wrap_err("failed to write completions to stdout")?;
    Ok(())
}
