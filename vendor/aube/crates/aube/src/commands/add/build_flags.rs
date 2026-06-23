use miette::{IntoDiagnostic, WrapErr, miette};
use std::path::Path;

/// Reject empty values for the allow-build flag with pnpm's
/// verbatim error message.
///
/// Catches the explicit empty form `--allow-build=`. The bare form
/// `--allow-build` is rejected upstream by clap (because the arg
/// has no `default_missing_value` and `require_equals = true`), so
/// it never reaches this validator.
///
/// Wording must stay byte-identical to pnpm's: scripts that grep
/// pnpm's stderr for this exact line continue to work after a swap
/// to aube.
pub(crate) fn parse_allow_build_value(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("The --allow-build flag is missing a package name. \
             Please specify the package name(s) that are allowed to run installation scripts."
            .to_string())
    } else {
        Ok(s.to_string())
    }
}

pub(super) fn parse_deny_build_value(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("The --deny-build flag is missing a package name. \
             Please specify the package name(s) that are denied from running installation scripts."
            .to_string())
    } else {
        Ok(s.to_string())
    }
}

pub(super) fn reject_conflicting_build_flags(
    allow_build: &[String],
    deny_build: &[String],
) -> miette::Result<()> {
    if allow_build.is_empty() || deny_build.is_empty() {
        return Ok(());
    }

    let mut overlap: Vec<&str> = allow_build
        .iter()
        .filter(|name| deny_build.contains(name))
        .map(String::as_str)
        .collect();
    overlap.sort_unstable();
    overlap.dedup();
    if overlap.is_empty() {
        return Ok(());
    }

    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_CONFLICTING_BUILD_FLAGS,
        "--allow-build and --deny-build both name the same package(s): {}. \
         Each package may only appear in one flag.",
        overlap.join(", ")
    ))
}

/// Apply `--allow-build=<pkg>` flags by writing each package as `true`
/// to the project's `allowBuilds` map (workspace yaml or
/// `package.json#aube.allowBuilds`), overwriting any prior value. An
/// explicit `false` is treated as something the user is now flipping
/// on purpose, not a conflict.
pub(super) fn apply_allow_build_flags(cwd: &Path, names: &[String]) -> miette::Result<()> {
    aube_manifest::workspace::add_to_allow_builds(cwd, names)
        .into_diagnostic()
        .wrap_err("failed to write --allow-build entries")?;
    Ok(())
}

/// Apply `--deny-build=<pkg>` flags by writing each package as `false`
/// to the project's `allowBuilds` map, overwriting any prior value.
pub(super) fn apply_deny_build_flags(cwd: &Path, names: &[String]) -> miette::Result<()> {
    aube_manifest::workspace::set_allow_builds(cwd, names, false)
        .into_diagnostic()
        .wrap_err("failed to write --deny-build entries")?;
    Ok(())
}
