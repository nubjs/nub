// `collapsible_if` fires on nested `if let { if let }` now that the workspace
// MSRV supports let chains; collapsing every site is cosmetic churn
// (and would diverge nub-native's verbatim get-tsconfig mirror), so allow it.
#![allow(clippy::collapsible_if)]

mod agent;
mod cli;
mod config;
mod dynamic_phantom;
mod init;
mod install_engine;
mod jsonc;
mod nubx_consent;
mod phantom_scan;
mod pm_engine;
mod project_config;
mod sandbox_bridge;
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
    // Leaked to `'static`: the capability lives for the whole process (held until
    // `process::exit`, which skips Drop anyway), and the build-jail interposition hook
    // — stored on the process-global engine context — must capture it beyond any stack
    // frame. The RuntimeCapability is opaque + non-cloneable, so leaking the single
    // instance is the clean way to give it the required lifetime.
    let sandbox_runtime: &'static nub_sandbox::RuntimeCapability =
        Box::leak(Box::new(nub_sandbox::earliest_bootstrap()?));

    if std::env::var_os("__NUB_VALIDATE_RESOURCE_BUNDLE").is_some() {
        nub_sandbox::validate_adjacent_resource_bundle()
            .map_err(|error| anyhow::anyhow!("invalid Nub resource bundle: {error}"))?;
    }

    // Engine-aware subscriber: surfaces the embedded engine's warning
    // channel (brand-rewritten) by default; RUST_LOG still owns the
    // filter when set. See pm_engine::log.
    pm_engine::log::init();

    // Interpose nub's build-jail around aube's dependency lifecycle-script spawns
    // (aube's own build jail is neutralized under the NUB profile). Installed once,
    // before any PM command runs; a non-install command never invokes the hook.
    pm_engine::build_jail::install(sandbox_runtime);

    // Latch the experimental project-grant arm from the environment, here — before any
    // command, and so before any dependency script exists to influence it. A set
    // variable this binary cannot honour ABORTS rather than degrading to the default
    // arm, and an active arm announces itself, so a study run can never measure one
    // configuration while believing it measured another. `nub_sandbox::arm` module doc.
    match nub_sandbox::arm::init_from_env() {
        Ok(decision) => {
            if let Some(banner) = decision.banner() {
                eprintln!("{banner}");
            }
        }
        Err(refusal) => anyhow::bail!(refusal),
    }

    let exit_code = cli::run(sandbox_runtime)?;
    std::process::exit(exit_code);
}
