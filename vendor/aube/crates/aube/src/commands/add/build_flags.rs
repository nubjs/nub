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
    if s.is_empty() || s == "!" {
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

/// Split `--allow-build` values by pnpm's selector: `pkg` approves,
/// `!pkg` denies (the same as `--deny-build=pkg`).
fn split_allow_build_selectors(allow_build: &[String]) -> (Vec<&str>, Vec<&str>) {
    let (denied, allowed): (Vec<&str>, Vec<&str>) = allow_build
        .iter()
        .map(String::as_str)
        .partition(|value| value.starts_with('!'));
    let denied = denied.into_iter().map(|value| &value[1..]).collect();
    (allowed, denied)
}

pub(super) fn reject_conflicting_build_flags(
    allow_build: &[String],
    deny_build: &[String],
) -> miette::Result<()> {
    let (allowed, mut denied) = split_allow_build_selectors(allow_build);
    denied.extend(deny_build.iter().map(String::as_str));
    if allowed.is_empty() || denied.is_empty() {
        return Ok(());
    }

    let mut overlap: Vec<&str> = allowed
        .iter()
        .copied()
        .filter(|name| denied.contains(name))
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

/// Apply `--allow-build` flags to the project's `allowBuilds` map
/// (workspace yaml or `package.json`): `pkg` writes `true`, `!pkg` writes
/// `false`. An approval never overturns a denial the project already
/// recorded — pnpm 12's `ERR_PNPM_OVERRIDING_IGNORED_BUILT_DEPENDENCIES`.
/// A flag on one command is easy to pass without looking at the config
/// it contradicts; changing a recorded decision is an edit to that entry.
pub(super) fn apply_allow_build_flags(cwd: &Path, values: &[String]) -> miette::Result<()> {
    let (allowed, denied) = split_allow_build_selectors(values);
    let recorded_denials = recorded_denials(cwd)?;
    let overridden: Vec<&str> = allowed
        .iter()
        .copied()
        .filter(|name| recorded_denials.contains(*name))
        .collect();
    if !overridden.is_empty() {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_OVERRIDING_IGNORED_BUILT_DEPENDENCIES,
            help = format!(
                "To let them run their build scripts, change their `{}` entries to `true`.",
                aube_manifest::allow_scripts_field_name()
            ),
            "The project denies builds for {}, which --allow-build would allow.",
            overridden.join(", ")
        ));
    }
    let as_owned = |names: Vec<&str>| names.into_iter().map(str::to_string).collect::<Vec<_>>();
    if !allowed.is_empty() {
        aube_manifest::workspace::add_to_allow_builds(cwd, &as_owned(allowed))
            .into_diagnostic()
            .wrap_err("failed to write --allow-build entries")?;
    }
    if !denied.is_empty() {
        aube_manifest::workspace::set_allow_builds(cwd, &as_owned(denied), false)
            .into_diagnostic()
            .wrap_err("failed to write --allow-build entries")?;
    }
    Ok(())
}

/// The package keys the project's build policy denies outright: a `false`
/// entry, a `neverBuiltDependencies` entry, or `dependenciesMeta.<pkg>.built:
/// false`.
fn recorded_denials(cwd: &Path) -> miette::Result<std::collections::BTreeSet<String>> {
    let manifest_path = cwd.join("package.json");
    let manifest = if manifest_path.is_file() {
        aube_manifest::PackageJson::from_path(&manifest_path)
            .map_err(miette::Report::new)
            .wrap_err("failed to read package.json")?
    } else {
        aube_manifest::PackageJson::default()
    };
    let workspace = aube_manifest::WorkspaceConfig::load(cwd)
        .map_err(miette::Report::new)
        .wrap_err("failed to load workspace config")?;
    let mut denied: std::collections::BTreeSet<String> = manifest
        .pnpm_allow_builds()
        .into_iter()
        .chain(workspace.allow_builds_raw())
        .filter(|(_, value)| matches!(value, aube_manifest::AllowBuildRaw::Bool(false)))
        .map(|(key, _)| key)
        .collect();
    denied.extend(manifest.pnpm_never_built_dependencies());
    denied.extend(workspace.never_built_dependencies.iter().cloned());
    denied.extend(manifest.dependencies_meta_built_false());
    Ok(denied)
}

/// Apply `--deny-build=<pkg>` flags by writing each package as `false`
/// to the project's `allowBuilds` map, overwriting any prior value.
pub(super) fn apply_deny_build_flags(cwd: &Path, names: &[String]) -> miette::Result<()> {
    aube_manifest::workspace::set_allow_builds(cwd, names, false)
        .into_diagnostic()
        .wrap_err("failed to write --deny-build entries")?;
    Ok(())
}
