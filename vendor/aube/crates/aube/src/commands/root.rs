//! `aube root` — print the path to `node_modules`.
//!
//! Mirrors `pnpm root` / `npm root`. Pure read: no filesystem mutation,
//! no network, no project lock. The directory doesn't have to exist yet.
//!
//! With `--global` / `-g`, prints the *global* package directory where
//! `aube add -g` installs live. Bins are symlinked out of it into
//! `aube bin -g` (a separate, PATH-visible directory).

use clap::Args;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube root
  /home/user/project/node_modules

  $ aube root -g
  /home/user/.local/share/aube/global/node_modules
";

#[derive(Debug, Args)]
pub struct RootArgs {
    /// Print the global package directory instead of the project's
    #[arg(short, long)]
    pub global: bool,
}

pub async fn run(args: RootArgs) -> miette::Result<()> {
    if args.global {
        let layout = super::global::GlobalLayout::resolve()?;
        println!("{}", layout.pkg_dir.display());
        return Ok(());
    }
    let cwd = crate::dirs::project_root_or_cwd()?;
    println!("{}", super::project_modules_dir(&cwd).display());
    Ok(())
}
