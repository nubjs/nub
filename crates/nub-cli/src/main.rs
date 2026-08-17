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
    // It precedes even the identity registration below because nub-sandbox has
    // zero aube dependency, so nothing on this path can derive a brand-scoped
    // path before the profile is set. It also precedes the environment restore
    // below: the handshake reads argv only and `process::exit`s on a sentinel,
    // so a monitor helper must never mutate its own environment on the way in.
    // Leaked to `'static`: the capability lives for the whole process (held until
    // `process::exit`, which skips Drop anyway), and the build-jail interposition hook
    // — stored on the process-global engine context — must capture it beyond any stack
    // frame. The RuntimeCapability is opaque + non-cloneable, so leaking the single
    // instance is the clean way to give it the required lifetime.
    let sandbox_runtime: &'static nub_sandbox::RuntimeCapability =
        Box::leak(Box::new(nub_sandbox::earliest_bootstrap()?));

    // A fresh invocation's ambient environment must be restored before config
    // discovery or logging can observe it, and the restore mutates the process
    // environment, which is only sound while nub is single-threaded.
    cli::normalize_invocation_environment();

    // Embedder identity before any line of nub that could reach the engine. Every
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

    // Tell the sandbox where nub keeps its data, so it can pick up a catalog UPDATE from that
    // directory instead of waiting for a nub release. Registration only — nothing is read until the
    // jail first needs a grant, and a run with no such file behaves exactly as before.
    //
    // ⛔ THE PATH IS RESOLVED HERE BECAUSE THIS IS WHERE EVERY OTHER NUB PATH DECISION LIVES. A second
    // resolver inside nub-sandbox would be a second answer to "where does nub keep its data", and the
    // two would drift — with the failure mode that an update lands in one place and is read from
    // another, so it silently never applies. `nub_sandbox::catalog_update` says the same thing.
    if let Some(data_dir) = pm_engine::nub_data_dir() {
        nub_sandbox::catalog_update::install(data_dir);
    }

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

    // Latch the development-only catalog override, on the same terms and for the same
    // reason as the arm above: before any command, so before any dependency script exists.
    // A set variable this binary cannot honour ABORTS; a catalog that fails to load falls
    // back to the compiled-in copy and SAYS SO, so a run always knows which catalog it
    // measured. `nub_sandbox::catalog_override` module doc.
    match nub_sandbox::catalog_override::init_from_env() {
        Ok(decision) => {
            if let Some(banner) = decision.banner() {
                eprintln!("{banner}");
            }
        }
        Err(refusal) => anyhow::bail!(refusal),
    }

    // And state which catalog the SHIPPED update tier settled on. Deliberately after the dev override
    // above, matching the precedence in `catalog_override::active_v2`, so a run that has both reads the
    // two lines in the order the tiers apply.
    //
    // ⛔ SILENT ONLY WHEN THERE IS NO FILE AT ALL. Almost nobody has one, and a line on every install
    // would train the reader to skip exactly the place a REFUSAL appears — and a refusal is
    // indistinguishable from tampering, so it is the one thing here that must be seen.
    if let Some(banner) = nub_sandbox::catalog_update::decision().banner() {
        eprintln!("{banner}");
    }

    let exit_code = cli::run(sandbox_runtime)?;
    std::process::exit(exit_code);
}
