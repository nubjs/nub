use super::{
    Location, NpmrcEdit, aube_config, is_npm_shared_key, resolve_aliases, setting_for_key,
};
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Setting key (canonical name or `.npmrc` alias).
    pub key: String,

    /// Value to write. Stored verbatim after `key=`.
    pub value: String,

    /// Shortcut for `--location project`.
    #[arg(long, conflicts_with = "location")]
    pub local: bool,

    /// Which config location to write to.
    ///
    /// Defaults to `user`. Writes land in `.npmrc` for the npm-shared
    /// surface — per-host auth/cert templates, scoped registries, and
    /// settings tagged `npmShared = true` in the settings registry
    /// (`registry`, `proxy` / `https-proxy`, `engine-strict`,
    /// `ignore-scripts`, etc.) — so npm and yarn read the same value.
    /// Aube-only and pnpm-only settings, plus unknown keys, land in
    /// aube's own config (`~/.config/aube/config.toml` at user scope,
    /// `<cwd>/.config/aube/config.toml` at project scope) where
    /// sibling tools don't see them.
    ///
    /// Dotted writes for aube map settings (`allowBuilds.<pkg>`,
    /// `overrides.<pkg>`, …) edit one entry at a time. At project
    /// scope (`--local`) they land in
    /// `pnpm-workspace.yaml#<map>.<entry>` or
    /// `package.json#aube.<map>.<entry>` if no workspace yaml exists,
    /// the same place install reads from. User-scope dotted writes
    /// for these maps error: aube only reads them per project.
    #[arg(long, value_enum, default_value_t = Location::User)]
    pub location: Location,
}

impl SetArgs {
    fn effective_location(&self) -> Location {
        if self.local {
            Location::Project
        } else {
            self.location
        }
    }
}

pub fn run(args: SetArgs) -> miette::Result<()> {
    set_value(&args.key, &args.value, args.effective_location(), true)
}

/// Write a NON-auth scalar setting to the project workspace yaml,
/// FORCE-creating the file if it doesn't exist yet.
///
/// This is the embedder seam for a host (nub) that, under a pnpm
/// incumbent, wants `config set <scalar>` to land in `pnpm-workspace.yaml`
/// for round-trip fidelity with pnpm — which always writes the workspace
/// yaml even when none exists yet, unlike [`set_value`]'s project-scope
/// path (step 5), which only diverts to a workspace yaml that already
/// exists on disk and otherwise falls back to `config.toml`.
///
/// `key` is a canonical setting name or `.npmrc`/yaml alias. Returns
/// `Ok(Some(path))` with the file written when the setting has a
/// top-level workspace-yaml key; `Ok(None)` when it has no workspace-yaml
/// mapping (the caller routes such keys elsewhere, e.g. the project
/// `.npmrc`). Object-typed (map) settings are rejected — they need
/// structural edits, not a scalar write — mirroring [`set_value`] step 3.
///
/// The workspace-yaml target name is resolved by
/// [`aube_manifest::workspace::workspace_yaml_target`], which under a
/// pnpm incumbent (the only context an embedder calls this in) is
/// `pnpm-workspace.yaml`. The typed value coercion and comment-preserving
/// write reuse [`aube_config::set_workspace_yaml_value`], so this is the
/// same on-disk result step 5 produces for an existing yaml.
pub fn set_project_scalar_to_workspace_yaml(
    key: &str,
    value: &str,
) -> miette::Result<Option<std::path::PathBuf>> {
    // Object-typed (map) settings can't be written as a single scalar.
    if let Some(meta) = setting_for_key(key)
        && meta.type_ == "object"
    {
        return Err(reject_aube_map_key(key, meta));
    }
    let Some(meta) = aube_config::is_aube_config_key(key) else {
        return Ok(None);
    };
    let Some(yaml_key) = aube_config::preferred_workspace_yaml_key(meta) else {
        return Ok(None);
    };
    let cwd = crate::dirs::project_root_or_cwd()?;
    let path = aube_manifest::workspace::workspace_yaml_target(&cwd);
    aube_config::set_workspace_yaml_value(&path, meta, yaml_key, value)?;
    Ok(Some(path))
}

pub(super) fn set_value(
    key: &str,
    value: &str,
    location: Location,
    report: bool,
) -> miette::Result<()> {
    // 1. Genuinely npm-shared keys (auth tokens, registries, npm
    //    scalars) keep their old `.npmrc` routing so npm/pnpm/yarn see
    //    the value. Everything else falls through to aube's own config.
    if is_npm_shared_key(key) {
        return write_npmrc(key, value, location, report);
    }

    // 2. Dotted writes that land in an aube map setting
    //    (`allowBuilds.<pkg>`, `overrides.<pkg>`, …). Project scope
    //    edits the workspace yaml or `package.json#aube.<map>`; user
    //    scope errors with a `--local` pointer (aube doesn't read
    //    user-scope maps yet).
    if let Some(handled) = try_set_aube_map_entry(key, value, location, report)? {
        return Ok(handled);
    }

    // 3. Bare object-typed aube settings (`allowBuilds` without a
    //    package name) can't be serialized as a single scalar.
    if let Some(meta) = setting_for_key(key)
        && meta.type_ == "object"
    {
        return Err(reject_aube_map_key(key, meta));
    }

    // 4. Dotted writes whose prefix is a *scalar* aube setting —
    //    scalar settings don't have a nested namespace.
    reject_scalar_nested_key(key)?;

    // 5. Known aube scalar setting → config.toml (or workspace yaml at
    //    project-scope, if one already exists).
    if let Some(meta) = aube_config::is_aube_config_key(key) {
        let path = aube_config_target(location, meta)?;
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml"))
            && let Some(yaml_key) = aube_config::preferred_workspace_yaml_key(meta)
        {
            aube_config::set_workspace_yaml_value(&path, meta, yaml_key, value)?;
            if report {
                eprintln!("set {}={} ({})", yaml_key, value, path.display());
            }
            return Ok(());
        }
        let mut edit = aube_config::AubeConfigEdit::load(&path)?;
        edit.set(meta, value)?;
        edit.save(&path)?;
        if report {
            eprintln!("set {}={} ({})", meta.name, value, path.display());
        }
        return Ok(());
    }

    // 6. Free-form unknown key — store as a TOML string in aube's own
    //    config rather than polluting the npm-shared `.npmrc`.
    let path = unknown_aube_config_target(location)?;
    let mut edit = aube_config::AubeConfigEdit::load(&path)?;
    edit.set_unknown(key, value);
    edit.save(&path)?;
    if report {
        eprintln!("set {}={} ({})", key, value, path.display());
    }
    Ok(())
}

/// Handle `aube config set <map>.<entry> <value>` when `<map>` is an
/// aube map setting. Returns `Ok(Some(()))` if the dotted form was
/// recognized (handled or rejected with a structured error),
/// `Ok(None)` if the key isn't a dotted aube-map write and the caller
/// should fall through to the normal flow.
///
/// Project-scope dotted writes land in the existing workspace yaml or
/// `package.json#<pnpm|aube>.<map>` via
/// [`aube_manifest::workspace::upsert_map_entry`] — the same path
/// `aube approve-builds` and install-time seeding take. User-scope
/// writes error with a `--local` pointer because aube only reads
/// these maps from the project workspace yaml / `package.json` today;
/// dropping a user-scope entry into `~/.config/aube/config.toml`
/// would be silently ineffective.
fn try_set_aube_map_entry(
    key: &str,
    value: &str,
    location: Location,
    report: bool,
) -> miette::Result<Option<()>> {
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
    // route through `is_aube_config_key` instead — they're scalar list/string
    // settings whose name happens to contain a dot.
    if aube_config::is_aube_config_key(key).is_some() {
        return Ok(None);
    }

    if !matches!(location, Location::Project) {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_CONFIG_NESTED_AUBE_KEY,
            help = format!(
                "use `{} --local {prefix}.{entry} {value}` to edit `pnpm-workspace.yaml#{prefix}.{entry}` (or `package.json#aube.{prefix}.{entry}` if no workspace yaml exists). Aube doesn't read user-scope `{prefix}` today.",
                aube_util::cmd("config set"),
            ),
            "`{key}` only applies at project scope: `{prefix}` is read from `pnpm-workspace.yaml` / `package.json#<pnpm|aube>.{prefix}`, not user-scope {} config.",
            aube_util::prog()
        ));
    }

    let (yaml_value, json_value) = scalar_to_yaml_json(value);
    let cwd = crate::dirs::project_root_or_cwd()?;
    let written =
        aube_manifest::workspace::upsert_map_entry(&cwd, meta.name, entry, yaml_value, json_value)
            .map_err(|e| miette!("failed to write {}.{entry}: {e}", meta.name))?;
    if report {
        eprintln!("set {}.{entry}={value} ({})", meta.name, written.display());
    }
    Ok(Some(()))
}

/// Parse a raw scalar string into matching yaml + json values for an
/// aube map entry. Only booleans get a typed representation, because
/// `allowBuilds` genuinely stores `true` / `false`; every other aube
/// map value (override versions, package-extension ranges, deprecated
/// version specs, …) must round-trip as a *string* so pnpm and
/// aube's typed-`String` serde fields deserialize cleanly. A bare
/// numeric version like `"4"` would otherwise serialize as a YAML /
/// JSON number and break the read side.
fn scalar_to_yaml_json(raw: &str) -> (yaml_serde::Value, serde_json::Value) {
    if let Some(b) = aube_settings::parse_bool(raw) {
        return (yaml_serde::Value::Bool(b), serde_json::Value::Bool(b));
    }
    (
        yaml_serde::Value::String(raw.to_string()),
        serde_json::Value::String(raw.to_string()),
    )
}

fn write_npmrc(key: &str, value: &str, location: Location, report: bool) -> miette::Result<()> {
    let aliases = resolve_aliases(key);
    let write_key = preferred_write_key(key, &aliases);
    let path = location.path()?;
    let mut edit = NpmrcEdit::load(&path)?;
    for alias in &aliases {
        if alias != &write_key {
            edit.remove(alias);
        }
    }
    edit.set(&write_key, value);
    edit.save(&path)?;
    if report {
        eprintln!("set {}={} ({})", write_key, value, path.display());
    }
    sweep_stale_aube_config(key, &aliases, location)?;
    Ok(())
}

/// Sweep stale `config.toml` entries for `key` after an `.npmrc`
/// write. Necessary for settings that overlap the npm-shared
/// allowlist *and* the known-aube-setting set (`engineStrict`,
/// `ignoreScripts`, `color`, `loglevel`, `httpsProxy`, …): the
/// resolver gives `config.toml` higher precedence than `.npmrc`, so a
/// previous `aube config set` that landed in `config.toml` would
/// silently shadow the new `.npmrc` value otherwise.
fn sweep_stale_aube_config(
    key: &str,
    aliases: &[String],
    location: Location,
) -> miette::Result<()> {
    let Some(meta) = aube_config::is_aube_config_key(key) else {
        return Ok(());
    };
    let config_path = match location {
        Location::User | Location::Global => aube_config::user_aube_config_path()?,
        Location::Project => {
            aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
        }
    };
    let mut edit = aube_config::AubeConfigEdit::load(&config_path)?;
    let mut sweep: Vec<String> = aliases.to_vec();
    if !sweep.iter().any(|s| s == meta.name) {
        sweep.push(meta.name.to_string());
    }
    if edit.remove_aliases(&sweep) {
        edit.save(&config_path)?;
    }
    Ok(())
}

fn reject_aube_map_key(key: &str, meta: &aube_settings::meta::SettingMeta) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_CONFIG_NESTED_AUBE_KEY,
        help = format!(
            "set a single entry with `{} --local {key}.<entry> <value>`, or edit `{key}:` directly in `pnpm-workspace.yaml` / `aube.{key}` in `package.json`.",
            aube_util::cmd("config set"),
        ),
        "`{key}` is an aube map setting (type `{}`) and can't be set as a single scalar — set one entry at a time, or edit the map structurally.",
        meta.type_,
    )
}

/// Where free-form (settings.toml-unknown) writes land. Unlike known
/// aube settings, unknowns never get workspace-yaml fallback — yaml
/// has no schema for arbitrary keys, and routing there would leak
/// random scalars into a file other tools read.
fn unknown_aube_config_target(location: Location) -> miette::Result<std::path::PathBuf> {
    match location {
        Location::User | Location::Global => aube_config::user_aube_config_path(),
        Location::Project => Ok(aube_config::project_aube_config_path(
            &crate::dirs::project_root_or_cwd()?,
        )),
    }
}

/// Decide where to write an aube-known setting for the given location.
/// Project-scope writes prefer an existing workspace yaml when no
/// project `config.toml` has been adopted yet — keeps the per-project
/// config story in a single file. Once `config.toml` exists, all
/// project writes go there (otherwise a yaml write would be silently
/// shadowed by the higher-precedence `config.toml` entry on read).
fn aube_config_target(
    location: Location,
    meta: &aube_settings::meta::SettingMeta,
) -> miette::Result<std::path::PathBuf> {
    match location {
        Location::User | Location::Global => aube_config::user_aube_config_path(),
        Location::Project => {
            let cwd = crate::dirs::project_root_or_cwd()?;
            let config_path = aube_config::project_aube_config_path(&cwd);
            if !config_path.exists()
                && aube_config::preferred_workspace_yaml_key(meta).is_some()
                && let Some(yaml_path) = aube_manifest::workspace::workspace_yaml_existing(&cwd)
            {
                return Ok(yaml_path);
            }
            Ok(config_path)
        }
    }
}

/// Reject `aube config set <scalar>.<sub> …` where `<scalar>` is a
/// known aube scalar setting (`autoInstallPeers`, `minimumReleaseAge`,
/// …). Object-typed prefixes are handled upstream by
/// [`try_set_aube_map_entry`]; this guard catches the remaining case
/// where the user typed a nested form against a setting that doesn't
/// have a nested namespace.
fn reject_scalar_nested_key(key: &str) -> miette::Result<()> {
    let Some((prefix, _)) = key.split_once('.') else {
        return Ok(());
    };
    let Some(meta) = setting_for_key(prefix) else {
        return Ok(());
    };
    // Object-typed prefixes are handled by `try_set_aube_map_entry`
    // before reaching here.
    if meta.type_ == "object" {
        return Ok(());
    }
    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_CONFIG_NESTED_AUBE_KEY,
        help = format!(
            "`{}` is type `{}` — set it directly with `{} {} <value>`.",
            meta.name,
            meta.type_,
            aube_util::cmd("config set"),
            meta.name,
        ),
        "`{key}` is not a writable config key: `{}` is a scalar aube setting and has no nested namespace.",
        meta.name,
    ))
}

pub(super) fn preferred_write_key(input: &str, aliases: &[String]) -> String {
    if aliases.iter().any(|a| a == input) {
        return input.to_string();
    }
    aliases
        .first()
        .cloned()
        .unwrap_or_else(|| input.to_string())
}
