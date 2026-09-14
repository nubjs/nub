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
mod config_fields;
mod dynamic_phantom;
mod env_owner;
mod fs_atomic;
mod init;
mod install_engine;
mod jsonc;
mod nubx_consent;
mod phantom_scan;
mod pm_engine;
mod prefix;
mod project_config;
mod self_shim;
mod verify_deps;

use anyhow::Result;

// nub binary only — keep out of crates/nub-native (the cdylib in Node).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    // First: a fresh invocation's ambient environment must be restored before
    // config discovery or logging can observe it, and the restore mutates the
    // process environment, which is only sound while nub is single-threaded.
    cli::normalize_invocation_environment();

    // Tracing stays silent unless RUST_LOG sets a filter. See pm_engine::log.
    pm_engine::log::init();

    let exit_code = cli::run()?;
    std::process::exit(exit_code);
}
