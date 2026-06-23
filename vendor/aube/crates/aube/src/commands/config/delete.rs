use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases, setting_for_key};
use miette::miette;
use std::path::{Path, PathBuf};

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    let location = args.effective_location();
    let meta = aube_config::is_aube_config_key(&args.key);
    // `set` routes npm-shared keys to `.npmrc` even when they're also
    // known aube settings (e.g. `engineStrict`, `strict-ssl`,
    // `ignore-scripts`). Delete must follow the same routing or it'll
    // skip the file the value actually lives in.
    let key_is_npm_shared = super::is_npm_shared_key(&args.key);

    // Dotted aube-map deletes (`allowBuilds.<pkg>`, `overrides.<pkg>`,
    // …) sweep workspace yaml / `package.json#aube.<map>` — symmetric
    // to `try_set_aube_map_entry`.
    if let Some(handled) = try_delete_aube_map_entry(&args.key, location)? {
        return Ok(handled);
    }

    let mut removed_paths: Vec<PathBuf> = Vec::new();

    // Project scope: sweep the workspace yaml first when the key is a
    // known aube setting. A setting can exist in both yaml and
    // `config.toml` (yaml-overrides-toml on read), so we never
    // short-circuit after a successful yaml removal.
    if let Some(meta) = meta
        && matches!(location, Location::Project)
        && let Some(yaml_path) =
            aube_manifest::workspace::workspace_yaml_existing(&crate::dirs::project_root_or_cwd()?)
        && aube_config::remove_workspace_yaml_aliases(&yaml_path, meta)?
    {
        removed_paths.push(yaml_path);
    }

    // Aube `config.toml`: sweep canonical name + every literal alias
    // + the raw key. The raw key covers free-form writes and the
    // canonical-name catch covers older configs that stored the key
    // under its `meta.name` even when the user typed an alias.
    let config_path = match location {
        Location::User | Location::Global => aube_config::user_aube_config_path()?,
        Location::Project => {
            aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
        }
    };
    let mut config_edit = aube_config::AubeConfigEdit::load(&config_path)?;
    let mut sweep: Vec<String> = aliases.clone();
    if let Some(meta) = meta
        && !sweep.iter().any(|s| s == meta.name)
    {
        sweep.push(meta.name.to_string());
    }
    if !sweep.iter().any(|s| s == &args.key) {
        sweep.push(args.key.clone());
    }
    if config_edit.remove_aliases(&sweep) {
        config_edit.save(&config_path)?;
        removed_paths.push(config_path.clone());
    }

    // `.npmrc`: sweep when the key is npm-shared (the canonical home
    // for those) or when it's a free-form / unknown key (which may
    // legitimately live in `.npmrc`). Aube-only settings
    // (`autoInstallPeers`, `minimumReleaseAge`, …) are intentionally
    // not swept — `.npmrc` is shared with npm/pnpm/yarn and an
    // aube-known entry there is typically a hand-edit the user wants
    // to control.
    let should_sweep_npmrc = key_is_npm_shared || meta.is_none();
    let npmrc_path = location.path()?;
    if should_sweep_npmrc && npmrc_path.exists() {
        let mut edit = NpmrcEdit::load(&npmrc_path)?;
        let mut removed = false;
        for alias in &aliases {
            if edit.remove(alias) {
                removed = true;
            }
        }
        if !aliases.iter().any(|a| a == &args.key) && edit.remove(&args.key) {
            removed = true;
        }
        if removed {
            edit.save(&npmrc_path)?;
            removed_paths.push(npmrc_path.clone());
        }
    }

    if removed_paths.is_empty() {
        // For aube-only settings (not in the npm-shared allowlist) we
        // intentionally skipped the `.npmrc` sweep — surface a stale
        // entry there so the user knows what's still shadowing the
        // delete. npm-shared and free-form keys fall through to the
        // simpler "not set anywhere" message.
        if let Some(_meta) = meta
            && !key_is_npm_shared
        {
            return Err(missing_aube_key_error(
                &args.key,
                &aliases,
                &config_path,
                location,
            ));
        }
        return Err(miette!(
            "{} not set in {} config or {}",
            args.key,
            aube_util::prog(),
            npmrc_path.display()
        ));
    }

    let joined = removed_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("deleted {} ({})", args.key, joined);
    Ok(())
}

/// Mirror of `try_set_aube_map_entry`: handle `aube config delete
/// <map>.<entry>` for an object-typed aube setting. Returns
/// `Ok(Some(()))` when the dotted form was recognized (and either
/// removed or rejected with a structured error), `Ok(None)` to fall
/// through to the normal delete flow. Project scope sweeps the
/// workspace yaml + `package.json#<pnpm|aube>.<map>`; user scope
/// errors with a `--local` pointer because aube only reads these
/// maps per project.
fn try_delete_aube_map_entry(key: &str, location: Location) -> miette::Result<Option<()>> {
    let Some((prefix, entry)) = key.split_once('.') else {
        return Ok(None);
    };
    let Some(meta) = setting_for_key(prefix) else {
        return Ok(None);
    };
    if meta.type_ != "object" {
        return Ok(None);
    }
    // Canonical dotted-name settings like `peerDependencyRules.allowedVersions`
    // are handled by the regular delete flow below (they're scalar
    // settings whose name happens to contain a dot).
    if aube_config::is_aube_config_key(key).is_some() {
        return Ok(None);
    }

    if !matches!(location, Location::Project) {
        return Err(miette!(
            "`{key}` only applies at project scope: `{prefix}` is read from `pnpm-workspace.yaml` / `package.json#<pnpm|aube>.{prefix}`, not user-scope {} config.\n\
             use `{} --local {prefix}.{entry}` to remove it from the project workspace yaml / `package.json`.",
            aube_util::prog(),
            aube_util::cmd("config delete"),
        ));
    }

    let cwd = crate::dirs::project_root_or_cwd()?;
    let removed = aube_manifest::workspace::remove_map_entry(&cwd, meta.name, entry)
        .map_err(|e| miette!("failed to remove {}.{entry}: {e}", meta.name))?;
    if !removed {
        return Err(miette!(
            "{}.{entry} not set in `pnpm-workspace.yaml` or `package.json`",
            meta.name,
        ));
    }
    eprintln!("deleted {}.{entry} ({})", meta.name, cwd.display());
    Ok(Some(()))
}

/// Build the error when an aube-only setting isn't in the expected
/// `config.toml`. Surfaces a still-present `.npmrc` entry so the user
/// knows where the value is coming from — aube doesn't modify
/// `.npmrc` for aube-only settings (it's shared with
/// npm/pnpm/yarn), so the message tells the user to clear the line
/// themselves.
fn missing_aube_key_error(
    key: &str,
    aliases: &[String],
    config_path: &Path,
    location: Location,
) -> miette::Report {
    if let Ok(npmrc_path) = location.path()
        && npmrc_path.exists()
        && let Ok(edit) = NpmrcEdit::load(&npmrc_path)
        && edit.entries().iter().any(|(k, _)| aliases.contains(k))
    {
        return miette!(
            "{key} is not set in {} but an entry exists in {}.\n\
             aube doesn't modify `.npmrc` for aube-only settings (it's shared with \
             npm/pnpm/yarn) — edit that file directly to remove it, or run \
             `{} {key} <value>` to override it from {}.",
            config_path.display(),
            npmrc_path.display(),
            aube_util::cmd("config set"),
            config_path.display(),
        );
    }
    miette!("{key} not set in {}", config_path.display())
}
