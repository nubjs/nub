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

  # From a workspace package, -w prints the workspace-root bin directory
  $ cd packages/app
  $ aube bin
  /home/user/project/packages/app/node_modules/.bin
  $ aube bin -w
  /home/user/project/node_modules/.bin

  # Extend PATH with the project bin directory
  $ export PATH=\"$(aube bin):$PATH\"
";

#[derive(Debug, Args)]
pub struct BinArgs {
    /// Print the global bin directory instead of the project's
    #[arg(short, long, conflicts_with = "workspace_root")]
    pub global: bool,

    /// Print the workspace-root bin directory instead of the current
    /// package's.
    ///
    /// Mirrors `pnpm bin -w`: from a sub-package, resolves the enclosing
    /// workspace root and prints its `node_modules/.bin`. No-op when no
    /// workspace root exists above cwd (single-project install), so the
    /// flag is safe to leave in shell aliases.
    #[arg(short = 'w', long = "workspace-root", visible_alias = "workspace")]
    pub workspace_root: bool,
}

pub async fn run(args: BinArgs) -> miette::Result<()> {
    if args.global {
        let layout = super::global::GlobalLayout::resolve()?;
        println!("{}", layout.bin_dir.display());
        return Ok(());
    }
    let mut cwd = crate::dirs::project_root_or_cwd()?;
    if args.workspace_root
        && let Some(root) = crate::dirs::find_workspace_root(&cwd)
    {
        cwd = root;
    }
    println!(
        "{}",
        super::project_modules_dir(&cwd).join(".bin").display()
    );
    Ok(())
}
