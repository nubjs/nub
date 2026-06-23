use crate::commands::{install, run::ScriptArgs, run::load_manifest, run::run_script};
use miette::miette;

/// `aube install-test` / `aube it` — pnpm-compat alias for `install && test`.
///
/// Unlike pnpm, aube auto-installs on every script invocation, so `aube test`
/// alone is equivalent. We emit a one-line hint pointing users at the shorter
/// form and still honor the explicit install so the command behaves the way
/// a pnpm muscle-memory user expects.
pub async fn run(script_args: ScriptArgs) -> miette::Result<Option<i32>> {
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

    eprintln!(
        "{}: `install-test` is redundant — {} auto-installs before scripts, \
         so `{}` on its own does the same thing.",
        aube_util::prog(),
        aube_util::prog(),
        aube_util::cmd("test")
    );

    // Fail fast when there's no `test` script so a project with a large
    // dependency graph doesn't eat a full install only to error out.
    let cwd = crate::dirs::project_root()?;
    let manifest = load_manifest(&cwd)?;
    if !manifest.scripts.contains_key("test") {
        return Err(miette!("script not found: test"));
    }

    if !no_install {
        let files = crate::commands::FileSources::load(&cwd);
        let raw_ws = aube_manifest::workspace::load_raw(&cwd)
            .map_err(|e| miette!("failed to load workspace config: {e}"))?;
        let env = aube_settings::values::capture_env();
        let ctx = files.ctx(&raw_ws, &env, &[]);
        let mode = install::FrozenMode::from_override(
            None,
            aube_settings::resolved::prefer_frozen_lockfile(&ctx),
        );
        // `install-test` is a pnpm-compat alias for `install && test`,
        // so the install phase needs to behave like argumentless `aube
        // install` — including running root lifecycle hooks. Override
        // the chained-call default that `with_mode()` sets.
        let mut opts = install::InstallOptions::with_mode(mode);
        opts.skip_root_lifecycle = false;
        install::run(opts).await?;
    }

    run_script(
        "test",
        &args,
        true,
        false,
        &aube_workspace::selector::EffectiveFilter::default(),
    )
    .await
}
