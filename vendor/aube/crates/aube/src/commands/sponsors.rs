#[derive(Debug, usage_rs::Args)]
pub struct SponsorsArgs;

pub async fn run(_args: SponsorsArgs) -> miette::Result<()> {
    println!(
        "aube and the jdx.dev open source tools are sponsored by:\n\n  entire.io - https://entire.io\n  Omacom Foundation - https://omarchy.org/patrons/\n\nView all sponsors: https://jdx.dev/sponsors.html"
    );
    Ok(())
}
