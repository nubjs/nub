use miette::miette;

use super::install;

pub(crate) async fn ensure_installed(no_install: bool) -> miette::Result<()> {
    if no_install {
        return Ok(());
    }
    if super::skip_auto_install_on_package_manager_mismatch() {
        return Ok(());
    }
    // Skip verify-deps when invoked from inside a script. The parent
    // install (or parent `aube run`) already validated freshness and
    // either holds the project lock or hasn't written `.aube-state` yet
    // — re-entering `ensure_installed` here would either deadlock on
    // the lock (`verifyDepsBeforeRun=install`) or hard-fail on the
    // missing state file (`verifyDepsBeforeRun=error`). Matches
    // npm/pnpm's "no verify-deps inside lifecycle scripts" contract.
    if std::env::var_os("npm_lifecycle_event").is_some() {
        return Ok(());
    }

    let initial_cwd = crate::dirs::cwd()?;
    // Prefer the workspace root as the freshness anchor. A monorepo
    // install writes its state files at the workspace root —
    // subpackages get symlinked `node_modules/` with no state file of
    // their own. Walking up only to the nearest `package.json` (the
    // subpackage itself) would miss that state and report "install
    // state not found" on every `aube run`/`exec`/`start` from a
    // subpackage even when the root install is fresh. Fall back to the
    // nearest `package.json` for non-workspace projects, and finally
    // to the cwd itself so we never panic resolving it.
    let cwd = crate::dirs::find_workspace_root(&initial_cwd)
        .or_else(|| crate::dirs::find_project_root(&initial_cwd))
        .unwrap_or(initial_cwd);
    // Resolve both pieces of auto-install policy in a single
    // `with_settings_ctx` call so the `.npmrc` + workspace-yaml read
    // pays off once. `aubeNoAutoInstall` lets a project/workspace opt
    // out of the staleness check entirely (env alias:
    // `AUBE_NO_AUTO_INSTALL`). `optimisticRepeatInstall=false`
    // disables the cheap lockfile/manifest hash short-circuit so every
    // check becomes a full install — matches pnpm's semantics where
    // the fast path is opt-out, not a staleness contract.
    let (skip_auto_install, optimistic_repeat) = super::with_settings_ctx(&cwd, |ctx| {
        (
            aube_settings::resolved::aube_no_auto_install(ctx),
            aube_settings::resolved::optimistic_repeat_install(ctx),
        )
    });
    if skip_auto_install {
        return Ok(());
    }
    let g = super::global_frozen_override();
    let needs = if optimistic_repeat {
        crate::state::check_needs_install(&cwd)
    } else {
        Some("optimisticRepeatInstall=false".to_string())
    };
    let verify_mode = resolve_verify_deps_before_run(&cwd)?;
    // A global `--frozen-lockfile` / `--no-frozen-lockfile` /
    // `--prefer-frozen-lockfile` re-triggers the install path even
    // when the state file says the tree is fresh, so the flag is
    // honored on every command that auto-installs.
    let Some(reason) = needs.or_else(|| g.map(|o| format!("global {} flag", o.cli_flag()))) else {
        return Ok(());
    };
    match verify_mode {
        VerifyDepsBeforeRun::Skip => return Ok(()),
        VerifyDepsBeforeRun::Warn => {
            eprintln!("Dependencies need install before run: {reason}");
            return Ok(());
        }
        VerifyDepsBeforeRun::Error => {
            return Err(miette!(
                "dependencies need install before run: {reason}\nRun `{}`, or set verifyDepsBeforeRun=install to let {} do it automatically.",
                aube_util::cmd("install"),
                aube_util::prog()
            ));
        }
        VerifyDepsBeforeRun::Install => {}
    }
    eprintln!("Auto-installing: {reason}");
    let mode = super::chained_frozen_mode(install::FrozenMode::Prefer);
    let mut opts = install::InstallOptions::with_mode(mode);
    opts.strict_no_lockfile = matches!(g, Some(install::FrozenOverride::Frozen));
    install::run(opts).await?;

    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum VerifyDepsBeforeRun {
    Install,
    Warn,
    Error,
    Skip,
}

fn resolve_verify_deps_before_run(cwd: &std::path::Path) -> miette::Result<VerifyDepsBeforeRun> {
    let files = super::FileSources::load(cwd);
    let empty_ws = std::collections::BTreeMap::new();
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&empty_ws, env, &[]);
    let raw = aube_settings::resolved::verify_deps_before_run(&ctx);
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "false" | "0" => VerifyDepsBeforeRun::Skip,
        "warn" => VerifyDepsBeforeRun::Warn,
        "error" => VerifyDepsBeforeRun::Error,
        "prompt" | "install" => VerifyDepsBeforeRun::Install,
        _ => VerifyDepsBeforeRun::Install,
    })
}
