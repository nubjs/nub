//! `aube view <pkg>[@version] [field]` — print package info from the registry.
//!
//! Mirrors `npm view` / `pnpm view`. Aliases: `info`, `show`. Fetches the
//! *full* packument (no corgi header, so description/repo/license/etc. are
//! preserved), resolves the target version from a tag or semver range, and
//! prints either a human-readable summary, a single dotted field, or the
//! raw JSON.
//!
//! This is a read-only command — no project lock, no manifest needed.

use crate::commands::{make_client, packument_full_cache_dir, resolve_version, split_name_spec};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use serde_json::Value;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube view react
  react@18.3.1 | MIT | deps: 1 | versions: 2037
  React is a JavaScript library for building user interfaces.
  https://react.dev/

  keywords: react

  dist
  .tarball: https://registry.npmjs.org/react/-/react-18.3.1.tgz
  .shasum:  a0b2eb79...
  .integrity: sha512-...

  # A specific version
  $ aube view react@17.0.2

  # A single field
  $ aube view react version
  18.3.1

  $ aube view react dist.tarball
  https://registry.npmjs.org/react/-/react-18.3.1.tgz

  # All versions ever published
  $ aube view react versions --json

  # Raw JSON for the resolved version
  $ aube view react@next --json
";

#[derive(Debug, Args)]
pub struct ViewArgs {
    /// Package to view, optionally with a version or dist-tag.
    ///
    /// Examples: `lodash`, `lodash@4.17.21`, `react@next`, `express@^4`.
    pub package: String,

    /// Dotted path into the version metadata to print.
    ///
    /// Examples: `version`, `dependencies`, `dist.tarball`,
    /// `maintainers.0.name`. When omitted, prints a formatted summary.
    pub field: Option<String>,

    /// Print the full JSON of the selected version instead of the summary.
    ///
    /// Mutually exclusive with `field`.
    #[arg(long, conflicts_with = "field")]
    pub json: bool,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: ViewArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let (name, version_spec) = split_name_spec(&args.package);
    let name = name.to_string();

    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    // Disk-backed cache with ETag/Last-Modified revalidation so repeated
    // `aube view` calls are cheap (near-instant within the TTL, 304 otherwise).
    let packument = client
        .fetch_packument_full_cached(&name, &packument_full_cache_dir())
        .await
        .map_err(|e| match e {
            aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
            other => miette!("failed to fetch {name}: {other}"),
        })?;

    let version = resolve_version(&packument, version_spec).ok_or_else(|| {
        miette!(
            "no matching version for {name}@{}",
            version_spec.unwrap_or("latest")
        )
    })?;

    let version_meta = packument
        .get("versions")
        .and_then(|v| v.get(&version))
        .ok_or_else(|| miette!("version {version} not present in packument for {name}"))?;

    if let Some(field) = &args.field {
        let value =
            dotted_get(version_meta, field).ok_or_else(|| miette!("field not found: {field}"))?;
        print_value(value);
        return Ok(());
    }

    if args.json {
        let json = serde_json::to_string_pretty(version_meta)
            .into_diagnostic()
            .wrap_err("failed to serialize packument")?;
        println!("{json}");
        return Ok(());
    }

    print_summary(&packument, version_meta, &name, &version);
    Ok(())
}

/// Walk a dotted path (`dist.tarball`, `maintainers.0.name`) into a JSON
/// value. Numeric segments index into arrays.
fn dotted_get<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
    for segment in path.split('.') {
        if let Ok(idx) = segment.parse::<usize>() {
            value = value.get(idx)?;
        } else {
            value = value.get(segment)?;
        }
    }
    Some(value)
}

/// Print a JSON value at the shell — strings unquoted, everything else as
/// pretty JSON. Matches `npm view`'s behavior for scalar vs structured
/// field lookups.
fn print_value(value: &Value) {
    match value {
        Value::String(s) => println!("{s}"),
        Value::Null => {}
        Value::Bool(_) | Value::Number(_) => println!("{value}"),
        _ => {
            let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            println!("{json}");
        }
    }
}

/// Human-readable summary: header line, description, homepage, keywords,
/// dist info, declared dependencies, maintainers, dist-tags.
fn print_summary(packument: &Value, version_meta: &Value, name: &str, version: &str) {
    // `license` is either a bare string (modern) or an object with a `type`
    // field (legacy) — look it up once and branch on the shape.
    let license = match version_meta.get("license") {
        Some(Value::String(s)) => s.as_str(),
        Some(obj) => obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown"),
        None => "unknown",
    };

    let deps_count = version_meta
        .get("dependencies")
        .and_then(|v| v.as_object())
        .map(|o| o.len())
        .unwrap_or(0);

    let versions_count = packument
        .get("versions")
        .and_then(|v| v.as_object())
        .map(|o| o.len())
        .unwrap_or(0);

    println!("{name}@{version} | {license} | deps: {deps_count} | versions: {versions_count}");

    if let Some(desc) = version_meta.get("description").and_then(|v| v.as_str())
        && !desc.is_empty()
    {
        println!();
        println!("{desc}");
    }

    if let Some(home) = version_meta.get("homepage").and_then(|v| v.as_str())
        && !home.is_empty()
    {
        println!("{home}");
    }

    if let Some(keywords) = version_meta.get("keywords").and_then(|v| v.as_array())
        && !keywords.is_empty()
    {
        let kws: Vec<String> = keywords
            .iter()
            .filter_map(|k| k.as_str().map(String::from))
            .collect();
        if !kws.is_empty() {
            println!();
            println!("keywords: {}", kws.join(", "));
        }
    }

    if let Some(bin) = version_meta.get("bin") {
        let names: Vec<String> = match bin {
            Value::String(_) => vec![name.split('/').next_back().unwrap_or(name).to_string()],
            Value::Object(map) => map.keys().cloned().collect(),
            _ => vec![],
        };
        if !names.is_empty() {
            println!();
            println!("bin: {}", names.join(", "));
        }
    }

    if let Some(dist) = version_meta.get("dist").and_then(|v| v.as_object()) {
        println!();
        println!("dist");
        for key in [
            "tarball",
            "shasum",
            "integrity",
            "unpackedSize",
            "fileCount",
        ] {
            if let Some(v) = dist.get(key) {
                let display = match v {
                    Value::String(s) => s.clone(),
                    _ => v.to_string(),
                };
                println!(".{key}: {display}");
            }
        }
    }

    if let Some(deps) = version_meta.get("dependencies").and_then(|v| v.as_object())
        && !deps.is_empty()
    {
        println!();
        println!("dependencies:");
        for (k, v) in deps {
            if let Some(range) = v.as_str() {
                println!("{k}: {range}");
            }
        }
    }

    if let Some(maintainers) = version_meta.get("maintainers").and_then(|v| v.as_array())
        && !maintainers.is_empty()
    {
        println!();
        println!("maintainers:");
        for m in maintainers {
            let who = m
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let email = m.get("email").and_then(|v| v.as_str()).unwrap_or("");
            if email.is_empty() {
                println!("- {who}");
            } else {
                println!("- {who} <{email}>");
            }
        }
    }

    if let Some(tags) = packument.get("dist-tags").and_then(|v| v.as_object())
        && !tags.is_empty()
    {
        println!();
        println!("dist-tags:");
        for (k, v) in tags {
            if let Some(s) = v.as_str() {
                println!("{k}: {s}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spec splitting is covered by `commands::split_name_spec_tests`; these
    // tests focus on the view-specific logic (version resolution, dotted
    // field traversal).

    #[test]
    fn resolve_version_defaults_to_latest_tag() {
        let p = serde_json::json!({
            "dist-tags": { "latest": "2.0.0", "next": "3.0.0-rc.1" },
            "versions": {
                "1.0.0": {}, "2.0.0": {}, "3.0.0-rc.1": {}
            }
        });
        assert_eq!(resolve_version(&p, None).as_deref(), Some("2.0.0"));
        assert_eq!(resolve_version(&p, Some("")).as_deref(), Some("2.0.0"));
    }

    #[test]
    fn resolve_version_follows_dist_tag() {
        let p = serde_json::json!({
            "dist-tags": { "latest": "2.0.0", "next": "3.0.0-rc.1" },
            "versions": {
                "1.0.0": {}, "2.0.0": {}, "3.0.0-rc.1": {}
            }
        });
        assert_eq!(
            resolve_version(&p, Some("next")).as_deref(),
            Some("3.0.0-rc.1")
        );
    }

    #[test]
    fn resolve_version_accepts_exact() {
        let p = serde_json::json!({
            "dist-tags": { "latest": "2.0.0" },
            "versions": { "1.0.0": {}, "2.0.0": {} }
        });
        assert_eq!(resolve_version(&p, Some("1.0.0")).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn resolve_version_semver_range_picks_highest_match() {
        let p = serde_json::json!({
            "dist-tags": { "latest": "2.3.1" },
            "versions": {
                "1.0.0": {}, "2.0.0": {}, "2.3.0": {}, "2.3.1": {}, "3.0.0": {}
            }
        });
        assert_eq!(resolve_version(&p, Some("^2")).as_deref(), Some("2.3.1"));
    }

    #[test]
    fn dotted_get_walks_object_and_array() {
        let v = serde_json::json!({
            "dist": { "tarball": "https://x" },
            "maintainers": [{ "name": "alice" }]
        });
        assert_eq!(
            dotted_get(&v, "dist.tarball").and_then(|v| v.as_str()),
            Some("https://x")
        );
        assert_eq!(
            dotted_get(&v, "maintainers.0.name").and_then(|v| v.as_str()),
            Some("alice")
        );
        assert!(dotted_get(&v, "missing").is_none());
    }
}
