//! `aube undeprecate <pkg-spec>` — clear an existing deprecation on the
//! registry. Wraps `deprecate::apply` with an empty message, which is
//! npm's convention for "remove the deprecated flag".

use crate::commands::{deprecate, split_name_spec};
use clap::Args;

#[derive(Debug, Args)]
pub struct UndeprecateArgs {
    /// Package spec: `name`, `name@version`, or `name@<range>`.
    ///
    /// Omitting the version clears the deprecation on every published
    /// version.
    pub package: String,

    /// Don't PUT anything — print which versions would be touched and exit.
    #[arg(long)]
    pub dry_run: bool,

    /// One-time password from a 2FA authenticator; sent as `npm-otp`.
    #[arg(long, value_name = "CODE")]
    pub otp: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: UndeprecateArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let (name, spec) = split_name_spec(&args.package);
    let name = name.to_string();
    let spec = spec.unwrap_or("*").to_string();
    deprecate::apply(
        &name,
        &spec,
        "",
        args.dry_run,
        args.otp.as_deref(),
        args.network.registry.as_deref(),
    )
    .await
}
