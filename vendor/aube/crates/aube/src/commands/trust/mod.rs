//! Commands for inspecting npm publishing trust.

mod check;

#[derive(Debug, usage_rs::Args)]
pub struct TrustArgs {
    #[usage(subcommand)]
    command: TrustCommand,
}

#[derive(Debug, usage_rs::Subcommands)]
enum TrustCommand {
    /// Check one package version for a publishing-trust downgrade
    #[usage(after_long_help = check::AFTER_LONG_HELP)]
    Check(check::CheckArgs),
}

pub async fn run(args: TrustArgs) -> miette::Result<()> {
    match args.command {
        TrustCommand::Check(args) => check::run(args).await,
    }
}
