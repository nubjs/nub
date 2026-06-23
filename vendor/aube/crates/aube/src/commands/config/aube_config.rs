use super::{literal_aliases, setting_for_key, settings_meta};
use crate::commands::npmrc::symlink_target_or_self;
use miette::{Context, IntoDiagnostic, miette};
use std::path::{Path, PathBuf};
use yaml_serde::Value as YamlValue;

pub(super) struct AubeConfigEdit {
    table: toml::map::Map<String, toml::Value>,
}

impl AubeConfigEdit {
    pub(super) fn load(path: &Path) -> miette::Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    table: toml::map::Map::new(),
                });
            }
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to read {}", path.display()));
            }
        };
        let table = toml::from_str::<toml::Table>(&raw)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
        Ok(Self { table })
    }

    pub(super) fn entries(&self) -> Vec<(String, String)> {
        self.table
            .iter()
            .filter_map(|(key, value)| toml_value_to_raw(value).map(|raw| (key.clone(), raw)))
            .collect()
    }

    pub(super) fn set(
        &mut self,
        meta: &settings_meta::SettingMeta,
        raw: &str,
    ) -> miette::Result<()> {
        let value = raw_to_toml_value(meta, raw)?;
        for alias in literal_aliases(meta.npmrc_keys) {
            self.table.remove(&alias);
        }
        self.table.insert(meta.name.to_string(), value);
        Ok(())
    }

    /// Store a free-form `key=value` pair as a TOML string. Used for
    /// keys that aren't in `settings.toml` and aren't part of the
    /// npm-shared `.npmrc` surface — they're aube-only by elimination,
    /// so they belong in aube's own config rather than `~/.npmrc`.
    pub(super) fn set_unknown(&mut self, key: &str, raw: &str) {
        self.table
            .insert(key.to_string(), toml::Value::String(raw.to_string()));
    }

    pub(super) fn remove_aliases(&mut self, aliases: &[String]) -> bool {
        let before = self.table.len();
        for alias in aliases {
            self.table.remove(alias);
        }
        before != self.table.len()
    }

    pub(super) fn save(&self, path: &Path) -> miette::Result<()> {
        let out = toml::to_string_pretty(&self.table)
            .into_diagnostic()
            .wrap_err(format!("failed to serialize {} config", aube_util::prog()))?;
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

pub(crate) fn user_aube_config_path() -> miette::Result<PathBuf> {
    if let Some(dir) = aube_util::env::xdg_config_home() {
        return Ok(dir.join("aube").join("config.toml"));
    }
    let home = aube_util::env::home_dir().ok_or_else(|| {
        miette!(
            "could not locate home directory. set HOME or USERPROFILE to point at {} config",
            aube_util::prog()
        )
    })?;
    Ok(home.join(".config").join("aube").join("config.toml"))
}

/// Project-scope aube config path: `<project>/.config/aube/config.toml`.
/// Mirrors the XDG layout used at user-scope so the same file name and
/// folder shape applies everywhere. Project-scope is an alternative to
/// committing aube-specific settings into a project `.npmrc` shared
/// with npm/pnpm/yarn.
pub(crate) fn project_aube_config_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".config").join("aube").join("config.toml")
}

pub(crate) fn load_user_entries() -> Vec<(String, String)> {
    let Ok(path) = user_aube_config_path() else {
        return Vec::new();
    };
    load_entries_at(&path)
}

pub(crate) fn load_project_entries(project_dir: &Path) -> Vec<(String, String)> {
    load_entries_at(&project_aube_config_path(project_dir))
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

fn raw_to_toml_value(meta: &settings_meta::SettingMeta, raw: &str) -> miette::Result<toml::Value> {
    match meta.type_ {
        "bool" => aube_settings::parse_bool(raw)
            .map(toml::Value::Boolean)
            .ok_or_else(|| miette!("{} expects a boolean value", meta.name)),
        "int" => raw
            .trim()
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| miette!("{} expects an integer value", meta.name)),
        "list<string>" => Ok(toml::Value::Array(
            parse_string_list(raw)
                .into_iter()
                .map(toml::Value::String)
                .collect(),
        )),
        _ => Ok(toml::Value::String(raw.to_string())),
    }
}

fn toml_value_to_raw(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Float(n) => Some(n.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Array(items) => {
            let values: Vec<String> = items.iter().filter_map(toml_value_to_raw).collect();
            Some(values.join(","))
        }
        toml::Value::Datetime(d) => Some(d.to_string()),
        toml::Value::Table(_) => None,
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
