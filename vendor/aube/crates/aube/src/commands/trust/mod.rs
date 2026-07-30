//! Commands for inspecting npm publishing trust.

mod check;

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct TrustArgs {
    #[command(subcommand)]
    command: TrustCommand,
}

#[derive(Debug, Subcommand)]
enum TrustCommand {
    /// Check one package version for a publishing-trust downgrade
    #[command(after_long_help = check::AFTER_LONG_HELP)]
    Check(check::CheckArgs),
}

pub async fn run(args: TrustArgs) -> miette::Result<()> {
    match args.command {
        TrustCommand::Check(args) => check::run(args).await,
    }
}
