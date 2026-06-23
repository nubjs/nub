//! `aube bin` — print the path to `node_modules/.bin`.
//!
//! Mirrors `pnpm bin` / `npm bin`. Shell scripts use it to extend `$PATH`
//! (`export PATH="$(aube bin):$PATH"`). No filesystem mutation, no network,
//! and the directory doesn't have to exist yet — we just print the path.
//!
//! With `--global` / `-g`, prints the *global* bin directory (the one a
//! user is expected to have on `$PATH` so globally-installed packages are
//! callable). See [`super::global`] for the layout.

use clap::Args;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube bin
  /home/user/project/node_modules/.bin

  $ aube bin -g
  /home/user/.local/share/aube/global/node_modules/.bin

  # Extend PATH with the project bin directory
  $ export PATH=\"$(aube bin):$PATH\"
";

#[derive(Debug, Args)]
pub struct BinArgs {
    /// Print the global bin directory instead of the project's
    #[arg(short, long)]
    pub global: bool,
}

pub async fn run(args: BinArgs) -> miette::Result<()> {
    if args.global {
        let layout = super::global::GlobalLayout::resolve()?;
        println!("{}", layout.bin_dir.display());
        return Ok(());
    }
    let cwd = crate::dirs::project_root_or_cwd()?;
    println!(
        "{}",
        super::project_modules_dir(&cwd).join(".bin").display()
    );
    Ok(())
}
