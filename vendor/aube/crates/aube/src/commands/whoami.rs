//! `aube whoami` — print the username associated with the configured
//! registry auth token. Mirrors `npm whoami` / `pnpm whoami`.
//!
//! Calls `GET {registry}/-/whoami` with the `.npmrc` bearer token. With no
//! token configured (or an invalid one) the registry returns 401, which
//! surfaces as an "authentication required" error pointing at `aube login`.

use clap::Args;
use miette::miette;

use crate::commands::make_client;

#[derive(Debug, Args)]
pub struct WhoamiArgs {
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: WhoamiArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    let username = client.fetch_whoami().await.map_err(|e| match e {
        aube_registry::Error::Unauthorized => {
            miette!(
                "not authenticated — run `{}` first",
                aube_util::cmd("login")
            )
        }
        other => miette!("failed to determine the current user: {other}"),
    })?;

    println!("{username}");
    Ok(())
}
