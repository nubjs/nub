mod cli;
mod pm_engine;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let exit_code = cli::run()?;
    std::process::exit(exit_code);
}
