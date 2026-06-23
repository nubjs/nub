use super::{ensure_installed, run::exec_optional, run::exec_script, run::load_manifest};
use crate::commands::run::{ScriptArgs, run_script};

/// `aube restart` — matches pnpm/npm semantics: if a `restart` script is
/// defined, run it; otherwise run `stop` then `start` (each optional, but
/// if neither exists we still succeed silently like pnpm does).
pub async fn run(
    script_args: ScriptArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    script_args.network.install_overrides();
    script_args.lockfile.install_overrides();
    script_args.virtual_store.install_overrides();
    let ScriptArgs {
        args,
        no_install,
        lockfile: _,
        network: _,
        virtual_store: _,
    } = script_args;
    // When a filter is present we only honor the explicit `restart` script
    // in each matched package — `run_script` handles workspace discovery,
    // sequential fanout, and the one-shot install. The `stop → start`
    // fallback path is intentionally single-project: restart-with-fallback
    // across a workspace is ambiguous (per-package? whole-workspace?) so
    // we defer it until someone asks for it.
    if !filter.is_empty() {
        return run_script("restart", &args, no_install, false, &filter).await;
    }
    let args = &args[..];
    let cwd = crate::dirs::project_root()?;
    let manifest = load_manifest(&cwd)?;

    // Mirror `run_script`: don't trigger auto-install unless there's actually
    // something to run. Otherwise `aube restart` in a project with no
    // lifecycle scripts would silently re-link the world for no reason.
    let has_any = manifest.scripts.contains_key("restart")
        || manifest.scripts.contains_key("stop")
        || manifest.scripts.contains_key("start");
    if !has_any {
        return Ok(None);
    }

    ensure_installed(no_install).await?;

    if manifest.scripts.contains_key("restart") {
        // Propagate a non-zero `restart` exit up to the binary's single
        // `std::process::exit` rather than terminating in place.
        if let Some(code) = exec_script(&cwd, &manifest, "restart", args).await? {
            return Ok(Some(code));
        }
    } else {
        // `stop` then `start`, each optional. A non-zero `stop` short-circuits
        // before `start` runs — matching the previous behavior where the inner
        // script runner terminated the process on the first failure.
        if let Some(Some(code)) = exec_optional(&cwd, &manifest, "stop", &[]).await? {
            return Ok(Some(code));
        }
        if let Some(Some(code)) = exec_optional(&cwd, &manifest, "start", args).await? {
            return Ok(Some(code));
        }
    }

    Ok(None)
}
