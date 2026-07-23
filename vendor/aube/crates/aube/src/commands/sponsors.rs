use clap::Args;

#[derive(Debug, Args)]
pub struct SponsorsArgs {}

pub async fn run(_args: SponsorsArgs) -> miette::Result<()> {
    println!(
        "aube and the jdx.dev open source tools are sponsored by:\n\n  entire.io - https://entire.io\n  37signals - https://37signals.com\n\nView all sponsors: https://jdx.dev/sponsors.html"
    );
    Ok(())
}
