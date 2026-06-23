//! `aube owner ls|add|rm <pkg> [<user>]` — manage package maintainers on
//! the registry. Mirrors `npm owner` / `pnpm owner`.
//!
//! - `ls <pkg>` — list maintainers (no auth needed for public packages).
//! - `add <pkg> <user>` / `rm <pkg> <user>` — read-modify-write the
//!   packument's `maintainers` array and PUT it back (the same authed
//!   full-document write `deprecate` uses). Requires auth.

use clap::{Args, Subcommand};
use miette::miette;

use crate::commands::make_client;

#[derive(Debug, Args)]
pub struct OwnerArgs {
    #[command(subcommand)]
    pub command: OwnerCommand,

    /// One-time password from a 2FA authenticator (for add/rm).
    #[arg(long, value_name = "CODE", global = true)]
    pub otp: Option<String>,

    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Subcommand)]
pub enum OwnerCommand {
    /// List the maintainers of a package.
    #[command(visible_alias = "list")]
    Ls { package: String },
    /// Add a maintainer to a package.
    Add { package: String, user: String },
    /// Remove a maintainer from a package.
    #[command(visible_alias = "remove")]
    Rm { package: String, user: String },
}

pub async fn run(args: OwnerArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);
    let otp = args.otp.as_deref();

    match args.command {
        OwnerCommand::Ls { package } => {
            let owners = client.fetch_owners(&package).await.map_err(map_err)?;
            if owners.is_empty() {
                eprintln!("no maintainers found for {package}");
                return Ok(());
            }
            for owner in owners {
                match owner.email {
                    Some(email) if !email.is_empty() => println!("{} <{}>", owner.name, email),
                    _ => println!("{}", owner.name),
                }
            }
        }
        OwnerCommand::Add { package, user } => {
            client
                .change_owner(&package, &user, true, otp)
                .await
                .map_err(map_err)?;
            // Drop the full-packument cache so a same-process `owner ls` /
            // `view` in the TTL window doesn't serve the pre-change document
            // (matches the sibling `deprecate` write path).
            client.invalidate_full_packument_cache(
                &package,
                &crate::commands::packument_full_cache_dir(),
            );
            println!("+{user}: {package}");
        }
        OwnerCommand::Rm { package, user } => {
            client
                .change_owner(&package, &user, false, otp)
                .await
                .map_err(map_err)?;
            client.invalidate_full_packument_cache(
                &package,
                &crate::commands::packument_full_cache_dir(),
            );
            println!("-{user}: {package}");
        }
    }
    Ok(())
}

fn map_err(e: aube_registry::Error) -> miette::Report {
    match e {
        aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
        aube_registry::Error::Unauthorized => {
            miette!(
                "not authenticated — run `{}` first",
                aube_util::cmd("login")
            )
        }
        aube_registry::Error::Forbidden { body } => {
            if body.is_empty() {
                miette!("registry rejected the request (insufficient permissions)")
            } else {
                miette!("registry rejected the request: {body}")
            }
        }
        other => miette!("{other}"),
    }
}
