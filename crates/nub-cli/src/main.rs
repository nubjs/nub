// `collapsible_if` fires on nested `if let { if let }` now that the workspace
// MSRV supports let chains; collapsing every site is cosmetic churn
// (and would diverge nub-native's verbatim get-tsconfig mirror), so allow it.
#![allow(clippy::collapsible_if)]

mod agent;
mod cli;
mod config;
mod dynamic_phantom;
mod init;
mod nubx_consent;
mod phantom_scan;
mod pm_engine;
mod project_config;
mod sandbox_redact;
mod self_shim;
mod verify_deps;

use anyhow::Result;

// nub binary only — keep out of crates/nub-native (the cdylib in Node).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    // Must be the first embedder action: Linux monitor mode is selected from a
    // private argv/descriptor handshake and never enters logging or CLI setup.
    let sandbox_runtime = nub_sandbox::earliest_bootstrap()?;

    if std::env::var_os("__NUB_VALIDATE_RESOURCE_BUNDLE").is_some() {
        nub_sandbox::validate_adjacent_resource_bundle()
            .map_err(|error| anyhow::anyhow!("invalid Nub resource bundle: {error}"))?;
    }

    // Engine-aware subscriber: surfaces the embedded engine's warning
    // channel (brand-rewritten) by default; RUST_LOG still owns the
    // filter when set. See pm_engine::log.
    pm_engine::log::init();

    let exit_code = cli::run(&sandbox_runtime)?;
    std::process::exit(exit_code);
}
