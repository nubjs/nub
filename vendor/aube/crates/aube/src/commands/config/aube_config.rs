use super::{literal_aliases, setting_for_key, settings_meta};
use crate::commands::npmrc::symlink_target_or_self;
use miette::{Context, IntoDiagnostic, miette};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value};
use yaml_serde::Value as YamlValue;

pub(super) struct AubeConfigEdit {
    document: DocumentMut,
}

impl AubeConfigEdit {
    pub(super) fn load(path: &Path) -> miette::Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    document: DocumentMut::new(),
                });
            }
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to read {}", path.display()));
            }
        };
        let document = raw
            .parse::<DocumentMut>()
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
        Ok(Self { document })
    }

    pub(super) fn entries(&self) -> Vec<(String, String)> {
        self.document
            .as_table()
            .iter()
            .filter_map(|(key, item)| {
                item.as_value()
                    .and_then(toml_value_to_raw)
                    .map(|raw| (key.to_string(), raw))
            })
            .collect()
    }

    pub(super) fn set(
        &mut self,
        meta: &settings_meta::SettingMeta,
        raw: &str,
    ) -> miette::Result<()> {
        let value = raw_to_toml_value(meta, raw)?;
        for alias in literal_aliases(meta.npmrc_keys) {
            if alias != meta.name {
                self.document.as_table_mut().remove(&alias);
            }
        }
        set_value_preserving_decor(self.document.as_table_mut(), meta.name, value);
        Ok(())
    }

    /// Store a free-form `key=value` pair as a TOML string. Used for
    /// keys that aren't in `settings.toml` and aren't part of the
    /// npm-shared `.npmrc` surface — they're aube-only by elimination,
    /// so they belong in aube's own config rather than `~/.npmrc`.
    pub(super) fn set_unknown(&mut self, key: &str, raw: &str) {
        set_value_preserving_decor(
            self.document.as_table_mut(),
            key,
            Value::from(raw.to_string()),
        );
    }

    pub(super) fn remove_aliases(&mut self, aliases: &[String]) -> bool {
        let table = self.document.as_table_mut();
        let before = table.len();
        for alias in aliases {
            table.remove(alias);
        }
        before != table.len()
    }

    pub(super) fn save(&self, path: &Path) -> miette::Result<()> {
        let out = self.document.to_string();
        // Follow symlinks so a user-managed `~/.config/aube/config.toml`
        // pointing at e.g. a dotfiles repo keeps its symlink intact;
        // atomic_write renames a sibling temp over the path, which
        // would otherwise replace the symlink with a regular file.
        let write_path = symlink_target_or_self(path).into_diagnostic()?;
        aube_util::fs_atomic::atomic_write(&write_path, out.as_bytes())
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write {}", write_path.display()))
    }
}

fn set_value_preserving_decor(table: &mut toml_edit::Table, key: &str, mut value: Value) {
    if let Some(item) = table.get_mut(key) {
        if let Some(existing) = item.as_value() {
            *value.decor_mut() = existing.decor().clone();
        }
        *item = Item::Value(value);
    } else {
        table.insert(key, Item::Value(value));
    }
}

/// User-scope branded config path: `~/.config/<dir>/config.toml`, where `<dir>`
/// is the active embedder's [`config_namespace`]. `Ok(None)` when the embedder
/// opts out of a branded user/project config file ([`config_namespace`] = `None`,
/// e.g. nub) — the tool then reads and writes no such file, keeping its config
/// surface on `.npmrc` + env. Standalone aube derives `~/.config/aube/config.toml`
/// byte-for-byte (its profile sets `Some("aube")`).
///
/// [`config_namespace`]: aube_util::Embedder::config_namespace
pub(crate) fn user_aube_config_path() -> miette::Result<Option<PathBuf>> {
    let Some(ns) = aube_util::embedder().config_namespace else {
        return Ok(None);
    };
    if let Some(dir) = aube_util::env::xdg_config_home() {
        return Ok(Some(dir.join(ns).join("config.toml")));
    }
    let home = aube_util::env::home_dir().ok_or_else(|| {
        miette!(
            "could not locate home directory. set HOME or USERPROFILE to point at {} config",
            aube_util::prog()
        )
    })?;
    Ok(Some(home.join(".config").join(ns).join("config.toml")))
}

/// Project-scope branded config path: `<project>/.config/<dir>/config.toml`,
/// where `<dir>` is the active embedder's [`config_namespace`]. Mirrors the XDG
/// layout used at user-scope. `None` when the embedder opts out
/// ([`config_namespace`] = `None`); standalone aube derives
/// `<project>/.config/aube/config.toml`. Project-scope is an alternative to
/// committing aube-specific settings into a project `.npmrc` shared with
/// npm/pnpm/yarn.
///
/// [`config_namespace`]: aube_util::Embedder::config_namespace
pub(crate) fn project_aube_config_path(project_dir: &Path) -> Option<PathBuf> {
    let ns = aube_util::embedder().config_namespace?;
    Some(project_dir.join(".config").join(ns).join("config.toml"))
}

/// Error for a config-file WRITE under a profile with no branded config file
/// ([`config_namespace`] = `None`). Unreachable for standalone aube (always
/// `Some`); an embedder like nub keeps settings on `.npmrc`/env, so a write that
/// targets the branded file has no destination.
///
/// [`config_namespace`]: aube_util::Embedder::config_namespace
fn no_branded_config_file_err() -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_NO_BRANDED_CONFIG_FILE,
        help = "this setting isn't part of the shared `.npmrc` surface; set it in `pnpm-workspace.yaml` / `package.json`, or via an environment variable.",
        "{prog} has no user/project config file — it stores settings in `.npmrc` and the environment, so this key can't be written to a config file.",
        prog = aube_util::prog(),
    )
}

/// The user-scope config-file path for a WRITE, erroring when the active profile
/// has no branded config file. Write paths use this; reads use
/// [`user_aube_config_path`] directly and skip on `None`.
pub(super) fn user_config_write_path() -> miette::Result<PathBuf> {
    user_aube_config_path()?.ok_or_else(no_branded_config_file_err)
}

/// The project-scope config-file path for a WRITE, erroring when the active
/// profile has no branded config file.
pub(super) fn project_config_write_path(project_dir: &Path) -> miette::Result<PathBuf> {
    project_aube_config_path(project_dir).ok_or_else(no_branded_config_file_err)
}

/// System-managed config path (`/etc/<dir>/managed.toml`), where `<dir>` is the
/// active embedder's [`managed_config_system_dir`]. `None` when the embedder
/// opts out of a system-managed surface — the tool then never reads a system
/// `/etc/<other>/managed.toml`, so a host cannot silently inherit another
/// tool's admin policy. Standalone aube derives `/etc/aube/managed.toml`
/// byte-for-byte (its profile sets `Some("aube")`).
///
/// [`managed_config_system_dir`]: aube_util::Embedder::managed_config_system_dir
pub(crate) fn system_managed_aube_config_path() -> Option<PathBuf> {
    aube_util::embedder()
        .managed_config_system_dir
        .map(|dir| PathBuf::from("/etc").join(dir).join("managed.toml"))
}

/// Name of the env var that overrides the managed-config path, branded to the
/// active embedder's [`config_env_prefix`] (e.g. `AUBE_MANAGED_CONFIG_PATH` for
/// standalone aube, `NUB_MANAGED_CONFIG_PATH` under nub). Built from the live
/// prefix so a warning names the var the tool actually reads and no static
/// `AUBE_`-branded token leaks under an embedder.
///
/// [`config_env_prefix`]: aube_util::Embedder::config_env_prefix
fn managed_config_env_var() -> String {
    let prefix = aube_util::embedder().config_env_prefix.unwrap_or("AUBE");
    format!("{prefix}_MANAGED_CONFIG_PATH")
}

pub(crate) fn load_managed_entries() -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(system_path) = system_managed_aube_config_path() {
        out.extend(load_entries_at(&system_path));
    }
    if let Some(path) = aube_util::env::config_env("MANAGED_CONFIG_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        match path.try_exists() {
            Ok(true) => out.extend(load_entries_at(&path)),
            Ok(false) => tracing::warn!(
                "managed config path from {} does not exist: {}",
                managed_config_env_var(),
                path.display()
            ),
            Err(err) => tracing::warn!(
                "failed to check managed config path from {} at {}: {err}",
                managed_config_env_var(),
                path.display()
            ),
        }
    }
    out
}

pub(crate) fn load_user_entries() -> Vec<(String, String)> {
    // `Ok(None)` (profile has no branded config file, e.g. nub) and `Err`
    // (no home dir) both yield no entries — the branded file is simply absent.
    let Ok(Some(path)) = user_aube_config_path() else {
        return Vec::new();
    };
    load_entries_at(&path)
}

pub(crate) fn load_project_entries(project_dir: &Path) -> Vec<(String, String)> {
    match project_aube_config_path(project_dir) {
        Some(path) => load_entries_at(&path),
        None => Vec::new(),
    }
}

fn load_entries_at(path: &Path) -> Vec<(String, String)> {
    match AubeConfigEdit::load(path) {
        Ok(edit) => edit.entries(),
        Err(err) => {
            tracing::warn!(
                "failed to load {} config at {}: {err}",
                aube_util::prog(),
                path.display()
            );
            Vec::new()
        }
    }
}

pub(super) fn is_aube_config_key(key: &str) -> Option<&'static settings_meta::SettingMeta> {
    let meta = setting_for_key(key)?;
    is_aube_config_setting(meta).then_some(meta)
}

/// Pick the workspace-yaml key to write under for this setting, or
/// `None` if the setting has no top-level workspace-yaml source.
/// Nested keys (e.g. `updateConfig.ignoreDependencies`) are skipped —
/// they require sub-mapping edits beyond the scope of a generic
/// `config set`.
pub(super) fn preferred_workspace_yaml_key(
    meta: &settings_meta::SettingMeta,
) -> Option<&'static str> {
    meta.workspace_yaml_keys
        .iter()
        .copied()
        .find(|k| !k.contains('.'))
}

/// Write `raw` to `key` in the workspace yaml at `path`, preserving
/// surrounding comments and unrelated keys via
/// [`aube_manifest::workspace::edit_workspace_yaml`].
pub(super) fn set_workspace_yaml_value(
    path: &Path,
    meta: &settings_meta::SettingMeta,
    key: &str,
    raw: &str,
) -> miette::Result<()> {
    let value = raw_to_yaml_value(meta, raw)?;
    aube_manifest::workspace::edit_workspace_yaml(path, |map| {
        map.insert(YamlValue::String(key.to_string()), value);
        Ok(())
    })
    .map_err(|e| miette!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

/// Remove every alias of `meta` from the workspace yaml at `path`.
/// Returns `true` if at least one key was found and removed.
pub(super) fn remove_workspace_yaml_aliases(
    path: &Path,
    meta: &settings_meta::SettingMeta,
) -> miette::Result<bool> {
    let aliases: Vec<&'static str> = meta
        .workspace_yaml_keys
        .iter()
        .copied()
        .filter(|k| !k.contains('.'))
        .collect();
    if aliases.is_empty() {
        return Ok(false);
    }
    let mut removed = false;
    aube_manifest::workspace::edit_workspace_yaml(path, |map| {
        for alias in &aliases {
            if map
                .shift_remove(YamlValue::String((*alias).to_string()))
                .is_some()
            {
                removed = true;
            }
        }
        Ok(())
    })
    .map_err(|e| miette!("failed to write {}: {e}", path.display()))?;
    Ok(removed)
}

fn raw_to_yaml_value(meta: &settings_meta::SettingMeta, raw: &str) -> miette::Result<YamlValue> {
    match meta.type_ {
        "bool" => aube_settings::parse_bool(raw)
            .map(YamlValue::Bool)
            .ok_or_else(|| miette!("{} expects a boolean value", meta.name)),
        "int" => raw
            .trim()
            .parse::<i64>()
            .map(|n| YamlValue::Number(n.into()))
            .map_err(|_| miette!("{} expects an integer value", meta.name)),
        "list<string>" => Ok(YamlValue::Sequence(
            parse_string_list(raw)
                .into_iter()
                .map(YamlValue::String)
                .collect(),
        )),
        _ => Ok(YamlValue::String(raw.to_string())),
    }
}

/// True when `meta` is a scalar-like aube setting that can round-trip
/// through `config.toml`. Object-typed maps (`allowBuilds`,
/// `overrides`, …) are excluded; the caller rejects those at the
/// `aube config set` boundary because they need structural edits in
/// workspace yaml / `package.json#aube.<name>` rather than a single
/// scalar TOML value.
///
/// The `typed_accessor_unused` flag is an audit hint for the workspace
/// accessor self-test, not a user-facing classification — settings like
/// `dangerouslyAllowAllBuilds` are still pure aube/pnpm-only knobs that
/// belong in `config.toml` rather than `.npmrc`.
fn is_aube_config_setting(meta: &settings_meta::SettingMeta) -> bool {
    matches!(
        meta.type_,
        "bool" | "string" | "path" | "url" | "int" | "list<string>"
    ) || meta.type_.starts_with('"')
}

fn raw_to_toml_value(meta: &settings_meta::SettingMeta, raw: &str) -> miette::Result<Value> {
    match meta.type_ {
        "bool" => aube_settings::parse_bool(raw)
            .map(Value::from)
            .ok_or_else(|| miette!("{} expects a boolean value", meta.name)),
        "int" => raw
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| miette!("{} expects an integer value", meta.name)),
        "list<string>" => {
            let mut array = toml_edit::Array::new();
            for item in parse_string_list(raw) {
                array.push(item);
            }
            Ok(Value::Array(array))
        }
        _ => Ok(Value::from(raw.to_string())),
    }
}

fn toml_value_to_raw(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.value().clone()),
        Value::Integer(n) => Some(n.value().to_string()),
        Value::Float(n) => Some(n.value().to_string()),
        Value::Boolean(b) => Some(b.value().to_string()),
        Value::Array(items) => {
            let values: Vec<String> = items.iter().filter_map(toml_value_to_raw).collect();
            Some(values.join(","))
        }
        Value::Datetime(d) => Some(d.value().to_string()),
        Value::InlineTable(_) => None,
    }
}

fn parse_string_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner
            .split(',')
            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aube_config_roundtrips_typed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let meta = settings_meta::find("minimumReleaseAge").unwrap();

        let mut edit = AubeConfigEdit::load(&path).unwrap();
        edit.set(meta, "2880").unwrap();
        edit.save(&path).unwrap();

        let edit = AubeConfigEdit::load(&path).unwrap();
        assert_eq!(
            edit.entries(),
            vec![("minimumReleaseAge".to_string(), "2880".to_string())]
        );
    }

    /// Default-preserving contract for the profile-derived managed-config
    /// paths: under the default (AUBE) profile the system path is exactly
    /// `/etc/aube/managed.toml` and the env-override var is exactly
    /// `AUBE_MANAGED_CONFIG_PATH`, byte-for-byte as before the path was routed
    /// through the embedder profile. This also exercises the SHARED derivation
    /// — `/etc/{dir}/managed.toml` from `managed_config_system_dir`, and
    /// `{prefix}_MANAGED_CONFIG_PATH` from `config_env_prefix` — so any other
    /// profile's path follows purely from its field values. The nub branch's
    /// field values (`managed_config_system_dir: Some("nub")` →
    /// `/etc/nub/managed.toml`, `config_env_prefix: Some("NUB")` →
    /// `NUB_MANAGED_CONFIG_PATH`) are pinned by the compile-time assertion in
    /// nub's `pm_engine::identity` and its end-to-end brand sweep; they can't be
    /// asserted here because registering a non-AUBE profile would flip the
    /// process-global `OnceLock` fallback this test depends on.
    #[test]
    fn managed_config_paths_are_aube_branded_under_default_profile() {
        assert_eq!(
            system_managed_aube_config_path(),
            Some(PathBuf::from("/etc/aube/managed.toml"))
        );
        assert_eq!(managed_config_env_var(), "AUBE_MANAGED_CONFIG_PATH");
    }

    /// Default-preserving contract for the profile-derived USER/PROJECT config
    /// paths: under the default (AUBE) profile the user path ends in
    /// `aube/config.toml` and the project path is exactly
    /// `<project>/.config/aube/config.toml`, byte-for-byte as before the path was
    /// routed through `config_namespace`. The `None` branch (nub: no branded
    /// config file → both paths absent, no `.config/aube/` read or write) can't be
    /// asserted here — registering a non-AUBE profile would flip the
    /// process-global `OnceLock` fallback every default-profile test depends on —
    /// so it's pinned by nub's `pm_engine::identity` compile-time assertion
    /// (`config_namespace.is_none()`) and the nub-side `pm_identity` brand sweep.
    #[test]
    fn user_project_config_paths_are_aube_branded_under_default_profile() {
        let user = user_aube_config_path()
            .expect("aube profile resolves a path")
            .expect("aube profile has a branded config file");
        assert!(
            user.ends_with("aube/config.toml"),
            "user path must live under the aube namespace: {}",
            user.display()
        );
        assert_eq!(
            project_aube_config_path(Path::new("/proj")),
            Some(PathBuf::from("/proj/.config/aube/config.toml"))
        );
    }

    #[test]
    fn save_preserves_comments_formatting_and_setting_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"# Security
strictDepBuilds = true # keep this rationale
minimumReleaseAge = 1440
trustPolicyExclude = [
    "example-a",
    # This package has no provenance yet.
    "example-b",
]

# Shell
scriptShell = 'C:\Program Files\git\bin\bash.exe'
"#;
        std::fs::write(&path, original).unwrap();

        let meta = settings_meta::find("minimumReleaseAge").unwrap();
        let mut edit = AubeConfigEdit::load(&path).unwrap();
        edit.set(meta, "2880").unwrap();
        edit.save(&path).unwrap();

        let expected = original.replacen("minimumReleaseAge = 1440", "minimumReleaseAge = 2880", 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn adding_setting_appends_without_reordering_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original =
            "# Deliberately not alphabetical\nstrictDepBuilds = true\nminimumReleaseAge = 2880\n";
        std::fs::write(&path, original).unwrap();

        let meta = settings_meta::find("trustPolicy").unwrap();
        let mut edit = AubeConfigEdit::load(&path).unwrap();
        edit.set(meta, "no-downgrade").unwrap();
        edit.save(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{original}trustPolicy = \"no-downgrade\"\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-config.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&real, "minimumReleaseAge = 1\n").unwrap();
        std::os::unix::fs::symlink("real-config.toml", &link).unwrap();

        let meta = settings_meta::find("minimumReleaseAge").unwrap();
        let mut edit = AubeConfigEdit::load(&link).unwrap();
        edit.set(meta, "2880").unwrap();
        edit.save(&link).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "save replaced the symlink instead of following it"
        );
        let written = std::fs::read_to_string(&real).unwrap();
        assert!(written.contains("minimumReleaseAge = 2880"));
    }
}
