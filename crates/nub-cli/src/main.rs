// `collapsible_if` fires on nested `if let { if let }` now that the workspace
// MSRV supports let chains; collapsing every site is cosmetic churn
// (and would diverge nub-native's verbatim get-tsconfig mirror), so allow it.
#![allow(clippy::collapsible_if)]

mod agent;
mod cli;
mod config;
mod config_fields;
mod dynamic_phantom;
mod env_owner;
mod init;
mod install_engine;
mod jsonc;
mod nubx_consent;
mod phantom_scan;
mod pm_engine;
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

    // Embedder identity before any subsystem initialization. Every
    // brand-scoped path the engine derives — cache root, data root, config
    // home — flows from `aube_util::embedder()`, which falls back to the
    // *aube* profile whenever the OnceLock is unset. Registering only inside
    // `engine_brand_preflight` left that fallback live on every non-PM path,
    // so `nub run` wrote the engine's node-gyp shim to `<cache>/aube/...` and
    // exported that path to scripts as `npm_config_node_gyp`. Registering here
    // makes the fallback structurally unreachable in the nub binary rather
    // than fixing one call site at a time; preflight still re-registers
    // (set-once, idempotent) so pm_engine stays self-contained under test.
    pm_engine::identity::register();

    // Engine-aware subscriber: surfaces the embedded engine's warning
    // channel (brand-rewritten) by default; RUST_LOG still owns the
    // filter when set. See pm_engine::log.
    pm_engine::log::init();

    let exit_code = cli::run()?;
    std::process::exit(exit_code);
}
