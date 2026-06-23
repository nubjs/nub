use std::path::Path;

use miette::{Context, IntoDiagnostic, miette};

/// Read and parse `package.json` at `manifest_path` with the standard
/// miette-wrapped error message used across commands.
pub(crate) fn load_manifest(manifest_path: &Path) -> miette::Result<aube_manifest::PackageJson> {
    aube_manifest::PackageJson::from_path(manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")
}

/// Load `<root>/package.json` when it exists, else return a default
/// (empty) manifest. Used by workspace-scoped commands that accept
/// yaml-only coordinator roots (`pnpm-workspace.yaml` only, no root
/// `package.json`).
pub(crate) fn load_manifest_or_default(root: &Path) -> miette::Result<aube_manifest::PackageJson> {
    let path = root.join("package.json");
    if path.is_file() {
        load_manifest(&path)
    } else {
        Ok(aube_manifest::PackageJson::default())
    }
}

/// Serialize `value` as pretty JSON with a trailing newline and
/// atomically write it to `path`. Wraps the serialize + atomic-write
/// pair used by add/remove/update/audit when mutating `package.json`.
pub(crate) fn write_manifest_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> miette::Result<()> {
    let json = serde_json::to_string_pretty(value)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    write_manifest_atomic(path, format!("{json}\n").as_bytes())
        .wrap_err("failed to write package.json")
}

pub(crate) fn update_manifest_json_object<F>(path: &Path, update: F) -> miette::Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> miette::Result<()>,
{
    let content = std::fs::read_to_string(path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;
    let mut json: serde_json::Value = serde_json::from_str(&content)
        .into_diagnostic()
        .wrap_err("failed to parse package.json")?;
    let serde_json::Value::Object(obj) = &mut json else {
        return Err(miette!("package.json must contain a JSON object"));
    };

    update(obj)?;

    let json = serde_json::to_string_pretty(&json)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    write_manifest_atomic(path, format!("{json}\n").as_bytes())
}

pub(crate) fn write_manifest_dep_sections(
    path: &Path,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<()> {
    update_manifest_json_object(path, |obj| {
        sync_manifest_dep_sections(obj, manifest);
        Ok(())
    })
}

fn sync_manifest_dep_sections(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    manifest: &aube_manifest::PackageJson,
) {
    sync_dep_section(obj, "dependencies", &manifest.dependencies);
    sync_dep_section(obj, "devDependencies", &manifest.dev_dependencies);
    sync_dep_section(obj, "peerDependencies", &manifest.peer_dependencies);
    sync_dep_section(obj, "optionalDependencies", &manifest.optional_dependencies);
}

fn sync_dep_section(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    deps: &std::collections::BTreeMap<String, String>,
) {
    if deps.is_empty() {
        obj.remove(key);
        return;
    }

    let section = deps
        .iter()
        .map(|(name, spec)| (name.clone(), serde_json::Value::String(spec.clone())))
        .collect();
    obj.insert(key.to_string(), serde_json::Value::Object(section));
}

/// Atomic write for `package.json` (and any sibling JSON we care
/// about): write to a tempfile in the same directory then rename.
/// The old `fs::write` truncates in place and a crash mid-write left
/// users with an empty manifest — the worst aube failure mode.
fn write_manifest_atomic(path: &Path, body: &[u8]) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(path, body)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_manifest_dep_sections_preserves_existing_top_level_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
  "name": "example",
  "version": "1.0.0",
  "license": "MIT",
  "scripts": {
    "test": "echo test"
  },
  "devDependencies": {
    "typescript": "^6.0.3"
  }
}
"#,
        )
        .unwrap();

        let mut manifest = aube_manifest::PackageJson::from_path(&path).unwrap();
        manifest
            .dev_dependencies
            .insert("tstyche".to_string(), "^7.1.0".to_string());

        write_manifest_dep_sections(&path, &manifest).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            root_key_order(&written),
            ["name", "version", "license", "scripts", "devDependencies"]
        );
        assert!(written.contains(r#""tstyche": "^7.1.0""#));
    }

    #[test]
    fn write_manifest_dep_sections_removes_empty_sections_without_reordering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
  "name": "example",
  "devDependencies": {
    "typescript": "^6.0.3"
  },
  "license": "MIT"
}
"#,
        )
        .unwrap();

        let mut manifest = aube_manifest::PackageJson::from_path(&path).unwrap();
        manifest.dev_dependencies.remove("typescript");

        write_manifest_dep_sections(&path, &manifest).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(root_key_order(&written), ["name", "license"]);
        assert!(!written.contains("devDependencies"));
    }

    fn root_key_order(raw: &str) -> Vec<String> {
        let serde_json::Value::Object(obj) = serde_json::from_str(raw).unwrap() else {
            panic!("expected object");
        };
        obj.keys().cloned().collect()
    }
}
