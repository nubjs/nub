//! `aube pkg get|set|delete|fix` — read and edit fields of the local
//! `package.json`. Mirrors `npm pkg` / `pnpm pkg` (`@pnpm/pkg-manifest`).
//!
//! - `get [<key> ...]` — print a field. One key prints the raw string
//!   value (or JSON when `--json`, or for non-string values); multiple
//!   keys print a JSON object keyed by the requested paths; no key prints
//!   the whole manifest. Missing single-key reads print an empty line.
//! - `set <key>=<value> ...` — set a field. `--json` parses each value as
//!   JSON (so `set private=true --json` writes a boolean, not a string).
//! - `delete <key> ...` — remove fields.
//! - `fix` — drop malformed `name`/`version`/dep-section/`bin` fields.
//!
//! Keys are dotted/bracketed property paths (`scripts.test`,
//! `contributors[0].name`); see [`super::property_path`].
//!
//! All edits go through [`super::update_manifest_json_object`], which
//! preserves top-level key order and writes atomically.

use clap::Args;
use miette::miette;
use serde_json::Value;

use super::property_path;

#[derive(Debug, Args)]
pub struct PkgArgs {
    /// Subcommand: `get`, `set`, `delete`, or `fix`.
    pub subcommand: String,

    /// Arguments for the subcommand: keys for `get`/`delete`, `key=value`
    /// pairs for `set`, none for `fix`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// For `set`, parse each value as JSON. For `get` of a single key,
    /// return its JSON-encoded form instead of the raw string.
    #[arg(long)]
    pub json: bool,

    /// Operate on the package.json in this directory (default: the
    /// nearest project root, or the cwd).
    #[arg(short = 'C', long, value_name = "DIR")]
    pub dir: Option<std::path::PathBuf>,
}

pub async fn run(args: PkgArgs) -> miette::Result<()> {
    let dir = match &args.dir {
        Some(d) => d.clone(),
        None => {
            crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."))
        }
    };
    let manifest_path = dir.join("package.json");

    match args.subcommand.as_str() {
        "get" => pkg_get(&manifest_path, &args.args, args.json),
        "set" => pkg_set(&manifest_path, &args.args, args.json),
        "delete" => pkg_delete(&manifest_path, &args.args),
        "fix" => pkg_fix(&manifest_path),
        other => Err(miette!(
            "unknown `pkg` subcommand {other:?} (expected get, set, delete, or fix)"
        )),
    }
}

fn read_value(manifest_path: &std::path::Path) -> miette::Result<Value> {
    let content = std::fs::read_to_string(manifest_path)
        .map_err(|e| miette!("failed to read {}: {e}", manifest_path.display()))?;
    serde_json::from_str(&content)
        .map_err(|e| miette!("failed to parse {}: {e}", manifest_path.display()))
}

fn pkg_get(manifest_path: &std::path::Path, keys: &[String], json: bool) -> miette::Result<()> {
    let manifest = read_value(manifest_path)?;

    if keys.len() == 1 {
        let segments = property_path::parse(&keys[0])?;
        match property_path::get(&manifest, &segments) {
            None => println!(),
            Some(value) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(value).unwrap_or_default()
                    );
                } else if let Value::String(s) = value {
                    println!("{s}");
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(value).unwrap_or_default()
                    );
                }
            }
        }
        return Ok(());
    }

    // Zero keys → whole manifest; multiple keys → object keyed by request.
    let selected = select_keys(&manifest, keys)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&selected).unwrap_or_default()
    );
    Ok(())
}

fn select_keys(manifest: &Value, keys: &[String]) -> miette::Result<Value> {
    if keys.is_empty() {
        return Ok(manifest.clone());
    }
    let mut out = serde_json::Map::new();
    for key in keys {
        let segments = property_path::parse(key)?;
        let value = property_path::get(manifest, &segments)
            .cloned()
            .unwrap_or(Value::Null);
        out.insert(key.clone(), value);
    }
    Ok(Value::Object(out))
}

fn pkg_set(manifest_path: &std::path::Path, args: &[String], json: bool) -> miette::Result<()> {
    if args.is_empty() {
        return Err(miette!("`pkg set` requires at least one key=value pair"));
    }
    super::update_manifest_json_object(manifest_path, |obj| {
        let mut root = Value::Object(std::mem::take(obj));
        for arg in args {
            let Some(eq) = arg.find('=') else {
                return Err(miette!(
                    "invalid argument {arg:?}: expected key=value format"
                ));
            };
            let (key, raw) = arg.split_at(eq);
            let raw = &raw[1..];
            let value = if json {
                serde_json::from_str(raw)
                    .map_err(|_| miette!("failed to parse value as JSON: {raw:?}"))?
            } else {
                Value::String(raw.to_string())
            };
            let segments = property_path::parse(key)?;
            property_path::set(&mut root, &segments, value)?;
        }
        if let Value::Object(map) = root {
            *obj = map;
        }
        Ok(())
    })
}

fn pkg_delete(manifest_path: &std::path::Path, keys: &[String]) -> miette::Result<()> {
    if keys.is_empty() {
        return Err(miette!("`pkg delete` requires at least one key"));
    }
    super::update_manifest_json_object(manifest_path, |obj| {
        let mut root = Value::Object(std::mem::take(obj));
        for key in keys {
            let segments = property_path::parse(key)?;
            property_path::delete(&mut root, &segments)?;
        }
        if let Value::Object(map) = root {
            *obj = map;
        }
        Ok(())
    })
}

fn pkg_fix(manifest_path: &std::path::Path) -> miette::Result<()> {
    super::update_manifest_json_object(manifest_path, |obj| {
        // name/version must be strings.
        if obj.get("name").is_some_and(|v| !v.is_string()) {
            obj.remove("name");
        }
        if obj.get("version").is_some_and(|v| !v.is_string()) {
            obj.remove("version");
        }
        // Dep sections and scripts must be plain objects.
        for field in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
            "scripts",
        ] {
            if obj.get(field).is_some_and(|v| !v.is_object()) {
                obj.remove(field);
            }
        }
        // bin must be a string or an object.
        if obj
            .get("bin")
            .is_some_and(|v| !v.is_string() && !v.is_object())
        {
            obj.remove("bin");
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("package.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn set_creates_nested_and_preserves_top_level_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_manifest(
            tmp.path(),
            "{\n  \"name\": \"x\",\n  \"version\": \"1.0.0\"\n}\n",
        );

        pkg_set(
            &path,
            &[
                "scripts.test=vitest".to_string(),
                "private=true".to_string(),
            ],
            false,
        )
        .unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["scripts"]["test"], "vitest");
        // Without --json, `true` is written as the string "true".
        assert_eq!(written["private"], "true");
        // Top-level order preserved (name, version come first).
        let keys: Vec<&String> = written.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "name");
        assert_eq!(keys[1], "version");
    }

    #[test]
    fn set_json_parses_value_types() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_manifest(tmp.path(), "{}\n");
        pkg_set(&path, &["private=true".to_string()], true).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["private"], serde_json::json!(true));
    }

    #[test]
    fn delete_removes_nested_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_manifest(
            tmp.path(),
            "{\n  \"scripts\": {\n    \"test\": \"vitest\",\n    \"build\": \"tsc\"\n  }\n}\n",
        );
        pkg_delete(&path, &["scripts.test".to_string()]).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(written["scripts"].get("test").is_none());
        assert_eq!(written["scripts"]["build"], "tsc");
    }

    #[test]
    fn fix_drops_malformed_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_manifest(
            tmp.path(),
            "{\n  \"name\": 5,\n  \"version\": \"1.0.0\",\n  \"scripts\": \"oops\"\n}\n",
        );
        pkg_fix(&path).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(written.get("name").is_none(), "non-string name dropped");
        assert_eq!(written["version"], "1.0.0", "valid version kept");
        assert!(
            written.get("scripts").is_none(),
            "non-object scripts dropped"
        );
    }
}
