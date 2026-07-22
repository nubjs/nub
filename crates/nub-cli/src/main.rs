// `collapsible_if` fires on nested `if let { if let }` now that the workspace
// MSRV supports let chains; collapsing every site is cosmetic churn
// (and would diverge nub-native's verbatim get-tsconfig mirror), so allow it.
#![allow(clippy::collapsible_if)]

mod agent;
mod cli;
// `nub compile` (spike): heavy compile-time deps (rolldown, libsui, zstd) live
// behind the `compile` feature so the default CLI build and CI cheap-gate matrix
// don't pay for them. The subcommand parses either way; without the feature its
// handler errors with a build hint.
#[cfg(feature = "compile")]
mod compile;
mod config;
mod dynamic_phantom;
mod init;
mod nubx_consent;
mod phantom_scan;
mod pm_engine;
mod self_shim;
mod verify_deps;

use anyhow::Result;

// nub binary only — keep out of crates/nub-native (the cdylib in Node).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    // Engine-aware subscriber: surfaces the embedded engine's warning
    // channel (brand-rewritten) by default; RUST_LOG still owns the
    // filter when set. See pm_engine::log.
    pm_engine::log::init();

    let exit_code = cli::run()?;
    std::process::exit(exit_code);
}
