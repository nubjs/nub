//! `aube prefix` — print the current package prefix directory.
//!
//! Mirrors `pnpm prefix`. Without flags, prints the current project root
//! (or cwd when no project root is found). With `--global` / `-g`, prints
//! the global prefix directory used for PATH-visible global bins.

use clap::Args;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube prefix
  /home/user/project

  $ aube prefix -g
  /home/user/.local/share/pnpm
";

#[derive(Debug, Args)]
pub struct PrefixArgs {
    /// Print the global prefix directory instead of the project's root
    #[arg(short, long)]
    pub global: bool,
}

pub async fn run(args: PrefixArgs) -> miette::Result<()> {
    if args.global {
        let prefix = super::global::prefix_dir()?;
        println!("{}", prefix.display());
        return Ok(());
    }
    let cwd = crate::dirs::project_root_or_cwd()?;
    println!("{}", cwd.display());
    Ok(())
}
