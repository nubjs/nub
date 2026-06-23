//! Generic yaml/json edit helpers for workspace-level config.
//!
//! `package.json#pnpm.<key>` mutations (`remove_setting_entry`,
//! `edit_setting_map`), workspace-yaml round-trip editing
//! (`edit_workspace_yaml`, `workspace_yaml_submap`,
//! `write_workspace_yaml`), and the `upsert_map_entry` /
//! `remove_map_entry` pair that routes through `config_write_target`
//! to mutate whichever file holds the value today.

use super::config::{ConfigWriteTarget, config_write_target, workspace_yaml_existing};
use super::yaml_patch;
use std::path::{Path, PathBuf};

/// The manifest config namespaces, compatible first (lower precedence),
/// this tool's own namespace last (wins on conflict). Standalone aube:
/// `["pnpm", "aube"]`.
fn config_namespaces() -> Vec<&'static str> {
    let id = aube_util::embedder();
    let mut ns: Vec<&'static str> = id.compatible_names.to_vec();
    if !id.manifest_namespace.is_empty() {
        ns.push(id.manifest_namespace);
    }
    ns
}

/// Drop `entry_key` from `pnpm.<key>` and `aube.<key>` in
/// `package.json`. Returns `Ok(true)` when at least one namespace held
/// it. Empty inner maps and empty namespaces are scrubbed too. The
/// rewrite is skipped entirely when nothing structural changes —
/// mirrors the no-op-skip guarantee of [`edit_workspace_yaml`].
///
/// Walking both namespaces matters because the read side merges them
/// (`aube.*` wins on conflict), so an entry recorded in either
/// location is live; a one-namespace removal would leave a stale
/// duplicate behind.
pub fn remove_setting_entry(cwd: &Path, key: &str, entry_key: &str) -> Result<bool, crate::Error> {
    let path = cwd.join("package.json");
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
    let mut value = crate::parse_json::<serde_json::Value>(&path, raw)?;
    let obj = value.as_object_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.clone(), "package.json is not an object".to_string())
    })?;
    let before = obj.clone();

    let mut existed = false;
    for ns in config_namespaces() {
        let mut ns_empty = false;
        if let Some(ns_obj) = obj.get_mut(ns).and_then(|v| v.as_object_mut()) {
            if let Some(inner) = ns_obj.get_mut(key).and_then(|v| v.as_object_mut()) {
                if inner.remove(entry_key).is_some() {
                    existed = true;
                }
                if inner.is_empty() {
                    ns_obj.remove(key);
                }
            }
            ns_empty = ns_obj.is_empty();
        }
        if ns_empty {
            obj.remove(ns);
        }
    }

    if *obj == before {
        return Ok(existed);
    }

    let mut out = serde_json::to_string_pretty(&value)
        .map_err(|e| crate::Error::YamlParse(path.clone(), format!("failed to serialize: {e}")))?;
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| crate::Error::Io(path, e))?;
    Ok(existed)
}

/// Mutate a namespaced map setting (e.g. `patchedDependencies`,
/// `allowBuilds`) inside `package.json` and write back.
///
/// The closure receives a **merged** view of `pnpm.<key>` and
/// `aube.<key>`, with `aube.*` winning on key conflict — the same
/// precedence the read side already uses. After the closure runs,
/// the merged result is written to a single namespace and the other
/// is cleared, so a future read sees exactly one source of truth and
/// can never silently shadow a stale entry. This matters because
/// pnpm-aware tools (and pnpm itself) can introduce a `pnpm` key into
/// a manifest after aube has already populated `aube.<key>`; without
/// the merge-and-collapse, a re-record would leave the new value in
/// `pnpm.<key>` while the stale `aube.<key>` entry kept winning on
/// read.
///
/// The chosen namespace follows [`config_write_target`]'s rule:
/// `pnpm` if a `pnpm` namespace is already declared in the manifest,
/// `aube` otherwise. Empty namespaces and inner maps are scrubbed,
/// and the rewrite is skipped entirely when nothing structural
/// changes — mirrors the no-op-skip guarantee of [`edit_workspace_yaml`].
pub fn edit_setting_map<F>(cwd: &Path, key: &str, f: F) -> Result<(), crate::Error>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>),
{
    let path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
    let mut value = crate::parse_json::<serde_json::Value>(&path, raw)?;

    let obj = value.as_object_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.clone(), "package.json is not an object".to_string())
    })?;
    let before = obj.clone();

    // Build the merged view (pnpm first, aube overrides on conflict)
    // before mutating, so the closure sees the same map the install
    // path would.
    let mut merged: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for ns in config_namespaces() {
        if let Some(inner) = obj
            .get(ns)
            .and_then(serde_json::Value::as_object)
            .and_then(|m| m.get(key))
            .and_then(serde_json::Value::as_object)
        {
            for (k, v) in inner {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    // For a manifest-root embedder (`manifest_namespace == ""`), the setting's
    // canonical home is a *top-level* `package.json` key — not in any namespace,
    // so it isn't covered by `config_namespaces()`. Fold it into the merged view
    // last (highest precedence) so an existing root-level map round-trips
    // instead of being clobbered by the write below.
    if aube_util::embedder().manifest_namespace.is_empty()
        && let Some(inner) = obj.get(key).and_then(serde_json::Value::as_object)
    {
        for (k, v) in inner {
            merged.insert(k.clone(), v.clone());
        }
    }

    f(&mut merged);

    // Pick the namespace to write into.
    //
    // An embedder whose `manifest_namespace` is `""` ("manifest root") writes
    // the setting at the **manifest root** unconditionally — `allowBuilds` /
    // `patchedDependencies` become top-level `package.json` keys, matching how
    // the embedder's own migration writer emits them (e.g. an embedder's
    // own `apply_manifest_edits`) and where the read side looks. It must NOT divert
    // to a pre-existing foreign-brand (`pnpm`) namespace, because under such an
    // embedder the read side gates that namespace off, so a nested write would
    // be orphaned. The empty namespace is signalled by `chosen_ns == None`.
    //
    // Otherwise: a compatible namespace already declared in the manifest wins
    // (`pnpm` for standalone aube with a pre-existing `pnpm` key); failing that,
    // this tool's own (non-empty) namespace. We never emit a literal `""` key.
    let id = aube_util::embedder();
    let chosen_ns: Option<&'static str> = if id.manifest_namespace.is_empty() {
        None
    } else {
        id.compatible_names
            .iter()
            .copied()
            .find(|ns| obj.contains_key(*ns))
            .or(Some(id.manifest_namespace))
    };

    // Drop `<key>` from every config namespace other than the chosen target so
    // the post-write state has a single source of truth. When the target is the
    // manifest root (`chosen_ns == None`), *every* namespace is an "other" and
    // gets scrubbed — the value lives at top level instead.
    for other_ns in config_namespaces() {
        if chosen_ns == Some(other_ns) {
            continue;
        }
        let mut other_ns_empty_after = false;
        if let Some(other_obj) = obj.get_mut(other_ns).and_then(|v| v.as_object_mut()) {
            other_obj.remove(key);
            other_ns_empty_after = other_obj.is_empty();
        }
        if other_ns_empty_after {
            obj.remove(other_ns);
        }
    }

    match chosen_ns {
        // Manifest-root target: read/insert/remove the setting key directly at
        // top level. Matches the embedder's migration writer (e.g. an embedder's
        // own `apply_manifest_edits`, which writes `allowBuilds`/`patchedDependencies`
        // at root and removes the `pnpm` object) and the read side, which skips
        // the empty self-namespace and (under a non-pnpm incumbent) the pnpm
        // namespace too — so a nested write would orphan.
        None => {
            if merged.is_empty() {
                obj.remove(key);
            } else {
                obj.insert(key.to_string(), serde_json::Value::Object(merged));
            }
        }
        // Namespaced target: write merged under the chosen namespace, or scrub
        // it if empty. Standalone aube (`manifest_namespace="aube"`) and any
        // embedder with a non-empty namespace land here unchanged.
        Some(chosen_ns) => {
            if merged.is_empty() {
                let mut chosen_ns_empty_after = false;
                if let Some(chosen_obj) = obj.get_mut(chosen_ns).and_then(|v| v.as_object_mut()) {
                    chosen_obj.remove(key);
                    chosen_ns_empty_after = chosen_obj.is_empty();
                }
                if chosen_ns_empty_after {
                    obj.remove(chosen_ns);
                }
            } else {
                let chosen_value = obj
                    .entry(chosen_ns.to_string())
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                let chosen_obj = chosen_value.as_object_mut().ok_or_else(|| {
                    crate::Error::YamlParse(path.clone(), format!("`{chosen_ns}` is not an object"))
                })?;
                chosen_obj.insert(key.to_string(), serde_json::Value::Object(merged));
            }
        }
    }

    if *obj == before {
        return Ok(());
    }

    let mut out = serde_json::to_string_pretty(&value)
        .map_err(|e| crate::Error::YamlParse(path.clone(), format!("failed to serialize: {e}")))?;
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| crate::Error::Io(path, e))?;
    Ok(())
}

/// Append `names` to the compatible-namespace `onlyBuiltDependencies`
/// array in `package.json` (`pnpm.onlyBuiltDependencies` under the nub /
/// standalone-aube profiles, whose `compatible_names` is `["pnpm"]`).
/// Creates the namespace object and the array as needed, dedupes against
/// existing entries, and skips the rewrite when nothing changes.
///
/// This is the heal-path write for the pnpm-compat/fresh surface
/// (`read_branded_pnpm_config` on, no workspace yaml on disk): it lands
/// the approval in pnpm's canonical *allowlist* key — the array form nearly
/// every real pnpm project uses — which the read side honors via
/// [`PackageJson::pnpm_only_built_dependencies`] and which real pnpm 10.x
/// reads from `package.json` too. Unlike [`edit_setting_map`], which under a
/// manifest-root embedder writes a *top-level* key, this targets the first
/// compatible namespace explicitly, because that is the surface the
/// pnpm-compat reader consults (the top-level key is gated off there).
///
/// Allowlist-only: `onlyBuiltDependencies` carries no per-entry boolean, so
/// only approvals (`allow=true`) route here. Denials use the
/// `pnpm.allowBuilds` map (see [`set_allow_builds`]).
///
/// [`set_allow_builds`]: super::mutations::set_allow_builds
pub fn add_to_pnpm_only_built_dependencies(
    cwd: &Path,
    names: &[String],
) -> Result<(), crate::Error> {
    // The compatible (pnpm) namespace is where the pnpm-compat reader looks.
    // For nub/aube this is `"pnpm"`; fall back to the tool's own namespace
    // only if no compatible name is declared (never the case for these
    // profiles, but keeps the helper total).
    let id = aube_util::embedder();
    let ns = id
        .compatible_names
        .first()
        .copied()
        .or((!id.manifest_namespace.is_empty()).then_some(id.manifest_namespace))
        .unwrap_or("pnpm");

    let path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
    let mut value = crate::parse_json::<serde_json::Value>(&path, raw)?;
    let obj = value.as_object_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.clone(), "package.json is not an object".to_string())
    })?;
    let before = obj.clone();

    let ns_obj = obj
        .entry(ns.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| crate::Error::YamlParse(path.clone(), format!("`{ns}` is not an object")))?;
    let arr = ns_obj
        .entry("onlyBuiltDependencies".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            crate::Error::YamlParse(
                path.clone(),
                format!("`{ns}.onlyBuiltDependencies` is not an array"),
            )
        })?;
    for name in names {
        let already = arr.iter().any(|v| v.as_str() == Some(name.as_str()));
        if !already {
            arr.push(serde_json::Value::String(name.clone()));
        }
    }

    if *obj == before {
        return Ok(());
    }
    let mut out = serde_json::to_string_pretty(&value)
        .map_err(|e| crate::Error::YamlParse(path.clone(), format!("failed to serialize: {e}")))?;
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| crate::Error::Io(path, e))?;
    Ok(())
}

/// Set `names` to `value` in the compatible-namespace `allowBuilds` map
/// in `package.json` (`pnpm.allowBuilds` under the nub / standalone-aube
/// profiles). Companion to [`add_to_pnpm_only_built_dependencies`] for the
/// denial (`false`) case, which the array allowlist can't represent.
///
/// Targets the first compatible namespace explicitly — same reasoning as
/// [`add_to_pnpm_only_built_dependencies`]: under a manifest-root embedder
/// on the pnpm-compat surface the reader consults `pnpm.allowBuilds`, not a
/// top-level `allowBuilds` key, so the write must be nested. Both nub
/// (via [`PackageJson::pnpm_allow_builds`]) and real pnpm 10.x read this map.
///
/// [`PackageJson::pnpm_allow_builds`]: crate::PackageJson::pnpm_allow_builds
pub fn set_pnpm_allow_builds_entries(
    cwd: &Path,
    names: &[String],
    value: bool,
) -> Result<(), crate::Error> {
    let id = aube_util::embedder();
    let ns = id
        .compatible_names
        .first()
        .copied()
        .or((!id.manifest_namespace.is_empty()).then_some(id.manifest_namespace))
        .unwrap_or("pnpm");

    let path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
    let mut json = crate::parse_json::<serde_json::Value>(&path, raw)?;
    let obj = json.as_object_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.clone(), "package.json is not an object".to_string())
    })?;
    let before = obj.clone();

    let ns_obj = obj
        .entry(ns.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| crate::Error::YamlParse(path.clone(), format!("`{ns}` is not an object")))?;
    let map = ns_obj
        .entry("allowBuilds".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            crate::Error::YamlParse(path.clone(), format!("`{ns}.allowBuilds` is not an object"))
        })?;
    for name in names {
        map.insert(name.clone(), serde_json::Value::Bool(value));
    }

    if *obj == before {
        return Ok(());
    }
    let mut out = serde_json::to_string_pretty(&json)
        .map_err(|e| crate::Error::YamlParse(path.clone(), format!("failed to serialize: {e}")))?;
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| crate::Error::Io(path, e))?;
    Ok(())
}

/// Upsert a single `<map>.<entry>` pair into the project's
/// workspace-level config. Routes through [`config_write_target`]:
/// workspace yaml when one exists, otherwise `<pnpm|aube>.<map>` in
/// `package.json`. Returns the file that was written.
///
/// Used by `aube config set --local <map>.<entry> <value>` for any
/// object-typed aube setting (`allowBuilds`, `overrides`,
/// `packageExtensions`, …) so the dotted-key CLI syntax can write
/// directly into the same maps `aube approve-builds` /
/// install-time auto-deny seeding mutate. The value is passed in
/// both yaml and json forms so the caller can choose the right scalar
/// shape (bool vs string vs int) without this helper having to guess.
pub fn upsert_map_entry(
    project_dir: &Path,
    map_name: &str,
    entry_key: &str,
    yaml_value: yaml_serde::Value,
    json_value: serde_json::Value,
) -> Result<PathBuf, crate::Error> {
    match config_write_target(project_dir) {
        ConfigWriteTarget::WorkspaceYaml(path) => {
            edit_workspace_yaml(&path, |map| {
                let submap = workspace_yaml_submap(map, map_name, &path)?;
                submap.insert(yaml_serde::Value::String(entry_key.to_string()), yaml_value);
                Ok(())
            })?;
            Ok(path)
        }
        ConfigWriteTarget::PackageJson => {
            edit_setting_map(project_dir, map_name, |map| {
                map.insert(entry_key.to_string(), json_value);
            })?;
            Ok(project_dir.join("package.json"))
        }
    }
}

/// Remove a single `<map>.<entry>` pair from the project's
/// workspace-level config. Mirrors [`upsert_map_entry`]: sweeps both
/// the workspace yaml (when one exists) and
/// `<pnpm|aube>.<map>.<entry>` in `package.json` so a value set
/// through either file can be deleted regardless of which one the
/// current layout would have written to. Drops empty `<map>:`
/// containers behind it so a removal doesn't leave a `{}` stub.
///
/// Returns `true` when at least one location held the entry. Used by
/// `aube config delete --local <map>.<entry>` so dotted writes have
/// a symmetric round-trip.
pub fn remove_map_entry(
    project_dir: &Path,
    map_name: &str,
    entry_key: &str,
) -> Result<bool, crate::Error> {
    let mut existed = false;
    if let Some(yaml_path) = workspace_yaml_existing(project_dir) {
        edit_workspace_yaml(&yaml_path, |map| {
            let yaml_key = yaml_serde::Value::String(map_name.to_string());
            let Some(submap) = map.get_mut(&yaml_key).and_then(|v| v.as_mapping_mut()) else {
                return Ok(());
            };
            if submap.shift_remove(entry_key).is_some() {
                existed = true;
            }
            if submap.is_empty() {
                map.shift_remove(&yaml_key);
            }
            Ok(())
        })?;
    }
    if remove_setting_entry(project_dir, map_name, entry_key)? {
        existed = true;
    }
    Ok(existed)
}

/// Get the inner mapping for a top-level workspace-yaml key, creating
/// it if absent. Errors when the key exists but isn't a mapping (a
/// hand-edited file shape we shouldn't silently replace).
pub(super) fn workspace_yaml_submap<'a>(
    map: &'a mut yaml_serde::Mapping,
    key: &str,
    path: &Path,
) -> Result<&'a mut yaml_serde::Mapping, crate::Error> {
    let entry = map
        .entry(yaml_serde::Value::String(key.to_string()))
        .or_insert_with(|| yaml_serde::Value::Mapping(yaml_serde::Mapping::new()));
    entry.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.to_path_buf(), format!("`{key}` must be a mapping"))
    })
}

/// Apply `f` to the parsed top-level mapping of the workspace yaml at
/// `path` and write it back. The helper exists so every workspace-yaml
/// writer (allowBuilds, patchedDependencies, catalog cleanup, future
/// settings) shares one comment-preserving rule: **user-authored
/// comments and formatting in the file survive every edit**.
///
/// The closure mutates a parsed `yaml_serde::Mapping`. After it runs,
/// the helper diffs before-vs-after and reduces the change set to a
/// minimal sequence of `yamlpatch` operations applied directly to the
/// original source. yamlpatch is comment- and format-preserving, so
/// keys, comments, and whitespace that the closure didn't touch land
/// back on disk byte-identical. A no-op closure produces an empty
/// patch list and the file isn't rewritten at all.
///
/// For brand-new or empty files there is no source to preserve, so the
/// helper falls back to `yaml_serde::to_string` for the initial write.
pub fn edit_workspace_yaml<F>(path: &Path, f: F) -> Result<PathBuf, crate::Error>
where
    F: FnOnce(&mut yaml_serde::Mapping) -> Result<(), crate::Error>,
{
    use yaml_serde::{Mapping, Value};

    let original_source: Option<String> = if path.exists() {
        let content =
            std::fs::read_to_string(path).map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
        if content.trim().is_empty() {
            None
        } else {
            Some(content)
        }
    } else {
        None
    };

    let mut doc: Value = match original_source.as_deref() {
        Some(content) => crate::parse_yaml(path, content.to_string())?,
        None => Value::Mapping(Mapping::new()),
    };

    let map = doc.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(
            path.to_path_buf(),
            "top-level yaml must be a mapping".to_string(),
        )
    })?;

    let before = map.clone();
    f(map)?;
    if *map == before {
        return Ok(path.to_path_buf());
    }

    let after = std::mem::take(map);
    write_workspace_yaml(path, original_source.as_deref(), &before, &after)?;
    Ok(path.to_path_buf())
}

/// Persist a structural change against `path`. When `original_source`
/// is `Some`, the change is encoded as a list of `yamlpatch`
/// operations applied to the original text — comments and formatting
/// the closure didn't touch survive the round trip. When it is `None`
/// (fresh file or one that was empty), the after-state is serialized
/// directly via `yaml_serde::to_string`; there is no source to
/// preserve. Both paths atomic-write the result.
fn write_workspace_yaml(
    path: &Path,
    original_source: Option<&str>,
    before: &yaml_serde::Mapping,
    after: &yaml_serde::Mapping,
) -> Result<(), crate::Error> {
    let bytes: Vec<u8> = match original_source {
        Some(source) => yaml_patch::apply_diff(path, source, before, after)?,
        None => {
            let raw = yaml_serde::to_string(&yaml_serde::Value::Mapping(after.clone()))
                .map_err(|e| crate::Error::YamlParse(path.to_path_buf(), e.to_string()))?;
            indent_block_sequences(&raw).into_bytes()
        }
    };
    aube_util::fs_atomic::atomic_write(path, &bytes)
        .map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
    Ok(())
}

/// Bump every block-sequence item line (`- ...`) by two spaces. Leaves
/// already-indented lines and non-sequence lines alone. yaml_serde's
/// output uses a single indent step per nesting level, so this produces
/// the `parent:\n  - item` shape humans expect. Only used on the
/// fresh-file write path; yamlpatch preserves the user's existing
/// indentation otherwise.
fn indent_block_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for line in input.split_inclusive('\n') {
        let stripped = line.trim_start_matches(' ');
        if stripped.starts_with("- ") || stripped == "-\n" || stripped == "-" {
            out.push_str("  ");
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join("package.json"), body).unwrap();
    }

    fn read_manifest(dir: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(dir.join("package.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    /// `edit_setting_map` writes the merged map into a real config namespace
    /// and never emits a literal empty-string key. Under the default (AUBE)
    /// profile with no `pnpm` namespace declared, the write lands in `aube`;
    /// `package.json[""]` must never exist. (The empty-`manifest_namespace`
    /// case — where the write target is the manifest root, not a namespace — is
    /// covered in the `root_namespace_write` integration test, since the active
    /// identity is once-per-process and can't be flipped inside this binary.)
    #[test]
    fn writes_into_a_real_namespace_never_empty_key() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(tmp.path(), "{\n  \"name\": \"x\"\n}\n");

        edit_setting_map(tmp.path(), "allowBuilds", |m| {
            m.insert("esbuild".to_string(), serde_json::Value::Bool(true));
        })
        .unwrap();

        let obj = read_manifest(tmp.path());
        let obj = obj.as_object().unwrap();
        assert!(
            !obj.contains_key(""),
            "edit_setting_map must never write an empty-string namespace key, got: {obj:#?}"
        );
        // Default profile, no pnpm namespace present → writes to `aube`.
        assert_eq!(
            obj["aube"]["allowBuilds"]["esbuild"],
            serde_json::Value::Bool(true)
        );
    }

    /// A pre-existing `pnpm` namespace is the chosen write target (pnpm-aware
    /// drop-in compatibility), and the stale value in the other namespace is
    /// scrubbed so reads see one source of truth.
    #[test]
    fn prefers_existing_pnpm_namespace_and_scrubs_others() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(
            tmp.path(),
            "{\n  \"name\": \"x\",\n  \"pnpm\": {},\n  \"aube\": { \"allowBuilds\": { \"old\": true } }\n}\n",
        );

        edit_setting_map(tmp.path(), "allowBuilds", |m| {
            m.insert("esbuild".to_string(), serde_json::Value::Bool(true));
        })
        .unwrap();

        let obj = read_manifest(tmp.path());
        // Merged map written into the chosen `pnpm` namespace…
        assert_eq!(
            obj["pnpm"]["allowBuilds"]["esbuild"],
            serde_json::Value::Bool(true)
        );
        // …and the previous value (`old`) carried over via the merge.
        assert_eq!(
            obj["pnpm"]["allowBuilds"]["old"],
            serde_json::Value::Bool(true)
        );
        // The non-chosen `aube` namespace is scrubbed of `allowBuilds`.
        assert!(obj.get("aube").and_then(|a| a.get("allowBuilds")).is_none());
    }
}
