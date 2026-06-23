//! `aube sbom` — emit a Software Bill of Materials for the installed graph.
//!
//! Reads the lockfile (no network, no linking), walks the root importer's
//! direct deps transitively, and serializes the closure as either CycloneDX
//! 1.5 JSON or SPDX 2.3 JSON. Pure read; does not touch `node_modules/` or
//! take the project lock.

use aube_lockfile::{LockedPackage, LockfileGraph};
use clap::Args;
use miette::{Context, IntoDiagnostic};
use std::collections::BTreeMap;

use super::DepFilter;

#[derive(Debug, Args)]
pub struct SbomArgs {
    /// Show only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Output format: `cyclonedx` (default) or `spdx`
    #[arg(long, value_enum, default_value_t = SbomFormat::Cyclonedx)]
    pub format: SbomFormat,

    /// Show only production dependencies (skip devDependencies)
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SbomFormat {
    Cyclonedx,
    Spdx,
}

pub async fn run(args: SbomArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let graph = super::load_graph(
        &cwd,
        &manifest,
        &format!(
            "no lockfile found — run `{}` before generating an SBOM",
            aube_util::cmd("install")
        ),
    )?;

    let filter = DepFilter::from_flags(args.prod, args.dev);
    let closure = super::collect_dep_closure(&graph, filter, false);

    let json = match args.format {
        SbomFormat::Cyclonedx => render_cyclonedx(&manifest, &closure)?,
        SbomFormat::Spdx => render_spdx(&manifest, &graph, filter, &closure)?,
    };

    println!("{json}");
    Ok(())
}

/// CycloneDX 1.5 JSON. See https://cyclonedx.org/docs/1.5/json/.
fn render_cyclonedx(
    manifest: &aube_manifest::PackageJson,
    closure: &BTreeMap<String, &LockedPackage>,
) -> miette::Result<String> {
    let root_name = manifest.name.clone().unwrap_or_else(|| "(unnamed)".into());
    let root_version = manifest.version.clone().unwrap_or_default();
    let root_ref = format!("{root_name}@{root_version}");

    let mut components = Vec::new();
    for (dep_path, pkg) in closure {
        let mut c = serde_json::Map::new();
        c.insert("type".into(), "library".into());
        c.insert("bom-ref".into(), dep_path.clone().into());
        c.insert("name".into(), pkg.name.clone().into());
        c.insert("version".into(), pkg.version.clone().into());
        c.insert("purl".into(), purl(&pkg.name, &pkg.version).into());
        components.push(serde_json::Value::Object(c));
    }

    let mut root_component = serde_json::Map::new();
    root_component.insert("type".into(), "application".into());
    root_component.insert("bom-ref".into(), root_ref.clone().into());
    root_component.insert("name".into(), root_name.into());
    if !root_version.is_empty() {
        root_component.insert("version".into(), root_version.clone().into());
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert("timestamp".into(), utc_now_iso8601().into());
    // CycloneDX 1.5 moved `metadata.tools` from a legacy tool-array to an
    // object with `components` / `services` sub-arrays. Emit the 1.5 shape.
    metadata.insert(
        "tools".into(),
        serde_json::json!({
            "components": [{
                "type": "application",
                "name": "aube",
                "version": env!("CARGO_PKG_VERSION"),
            }]
        }),
    );
    metadata.insert(
        "component".into(),
        serde_json::Value::Object(root_component),
    );

    let bom = serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": metadata,
        "components": components,
    });

    serde_json::to_string_pretty(&bom)
        .into_diagnostic()
        .wrap_err("failed to serialize CycloneDX SBOM")
}

/// SPDX 2.3 JSON. See https://spdx.github.io/spdx-spec/v2.3/.
fn render_spdx(
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    filter: DepFilter,
    closure: &BTreeMap<String, &LockedPackage>,
) -> miette::Result<String> {
    let root_name = manifest.name.clone().unwrap_or_else(|| "(unnamed)".into());
    let root_version = manifest.version.clone().unwrap_or_default();
    let root_spdx_id = "SPDXRef-Root".to_string();

    let mut packages = Vec::new();
    let mut root_pkg = serde_json::Map::new();
    root_pkg.insert("SPDXID".into(), root_spdx_id.clone().into());
    root_pkg.insert("name".into(), root_name.clone().into());
    if !root_version.is_empty() {
        root_pkg.insert("versionInfo".into(), root_version.clone().into());
    }
    root_pkg.insert("downloadLocation".into(), "NOASSERTION".into());
    root_pkg.insert("filesAnalyzed".into(), false.into());
    // SPDX 2.3 §7.13/§7.15/§7.17 require these for every package, including
    // the root. We don't read license info from the store yet, so everything
    // is NOASSERTION.
    root_pkg.insert("licenseConcluded".into(), "NOASSERTION".into());
    root_pkg.insert("licenseDeclared".into(), "NOASSERTION".into());
    root_pkg.insert("copyrightText".into(), "NOASSERTION".into());
    packages.push(serde_json::Value::Object(root_pkg));

    let mut relationships = Vec::new();
    // DESCRIBES: document -> root
    relationships.push(serde_json::json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relatedSpdxElement": root_spdx_id,
        "relationshipType": "DESCRIBES",
    }));

    // Index dep_path -> SPDXID so relationships can cross-reference.
    let mut id_map: BTreeMap<String, String> = BTreeMap::new();
    for dep_path in closure.keys() {
        id_map.insert(
            dep_path.clone(),
            format!("SPDXRef-Package-{}", sanitize_spdx_id(dep_path)),
        );
    }

    for (dep_path, pkg) in closure {
        let spdx_id = &id_map[dep_path];
        let mut p = serde_json::Map::new();
        p.insert("SPDXID".into(), spdx_id.clone().into());
        p.insert("name".into(), pkg.name.clone().into());
        p.insert("versionInfo".into(), pkg.version.clone().into());
        p.insert("downloadLocation".into(), "NOASSERTION".into());
        p.insert("filesAnalyzed".into(), false.into());
        p.insert("licenseConcluded".into(), "NOASSERTION".into());
        p.insert("licenseDeclared".into(), "NOASSERTION".into());
        p.insert("copyrightText".into(), "NOASSERTION".into());
        p.insert(
            "externalRefs".into(),
            serde_json::json!([{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl",
                "referenceLocator": purl(&pkg.name, &pkg.version),
            }]),
        );
        packages.push(serde_json::Value::Object(p));
    }

    // Root → direct deps (DEPENDS_ON). Walk the lockfile's root_deps list so
    // SPDXRef-Root actually has outgoing edges — iterating id_map alone only
    // captures inter-package edges and leaves the root orphaned.
    for direct in graph.root_deps() {
        if !filter.keeps(direct.dep_type) {
            continue;
        }
        if let Some(dep_id) = id_map.get(&direct.dep_path) {
            relationships.push(serde_json::json!({
                "spdxElementId": root_spdx_id,
                "relatedSpdxElement": dep_id,
                "relationshipType": "DEPENDS_ON",
            }));
        }
    }

    // Every closure package → its own transitive deps.
    for (dep_path, child_id) in &id_map {
        let child_pkg = closure[dep_path];
        for (name, version) in &child_pkg.dependencies {
            let child_dep_path = format!("{name}@{version}");
            if let Some(grandchild_id) = id_map.get(&child_dep_path) {
                relationships.push(serde_json::json!({
                    "spdxElementId": child_id,
                    "relatedSpdxElement": grandchild_id,
                    "relationshipType": "DEPENDS_ON",
                }));
            }
        }
    }

    // Capture one timestamp so namespace and creationInfo can't drift across
    // a second boundary. Namespace gets a nanosecond suffix so back-to-back
    // runs in the same second still produce distinct URIs as SPDX 2.3
    // requires.
    let (created, nanos) = now_iso8601_with_nanos();
    let namespace = format!(
        "https://aube.jdx.dev/spdx/{}-{}-{}.{:09}",
        root_name.replace('/', "_"),
        if root_version.is_empty() {
            "0.0.0"
        } else {
            &root_version
        },
        created,
        nanos,
    );

    let doc = serde_json::json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": root_name,
        "documentNamespace": namespace,
        "creationInfo": {
            "created": created,
            "creators": [format!("Tool: aube-{}", env!("CARGO_PKG_VERSION"))],
        },
        "packages": packages,
        "relationships": relationships,
    });

    serde_json::to_string_pretty(&doc)
        .into_diagnostic()
        .wrap_err("failed to serialize SPDX SBOM")
}

/// Build a purl for an npm package. Scoped names encode the leading `@` as
/// `%40` per the purl spec.
fn purl(name: &str, version: &str) -> String {
    if let Some(rest) = name.strip_prefix('@') {
        format!("pkg:npm/%40{rest}@{version}")
    } else {
        format!("pkg:npm/{name}@{version}")
    }
}

/// SPDXID locals must match `[A-Za-z0-9.\-]+`. Replace everything else with `-`.
fn sanitize_spdx_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// ISO 8601 UTC "YYYY-MM-DDTHH:MM:SSZ". Implemented via Howard Hinnant's
/// civil_from_days so we avoid pulling in `chrono` / `jiff` for one
/// timestamp. Valid for any Unix time the host clock can report.
fn utc_now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_unix_utc(secs)
}

/// Same as `utc_now_iso8601` but also returns the sub-second nanosecond
/// component, so callers can stitch it into a unique-per-invocation
/// identifier without a second `SystemTime::now()` call.
fn now_iso8601_with_nanos() -> (String, u32) {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (format_unix_utc(dur.as_secs() as i64), dur.subsec_nanos())
}

fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let hour = tod / 3600;
    let minute = (tod / 60) % 60;
    let second = tod % 60;

    // Howard Hinnant, http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purl_plain() {
        assert_eq!(purl("lodash", "4.17.21"), "pkg:npm/lodash@4.17.21");
    }

    #[test]
    fn purl_scoped() {
        assert_eq!(purl("@babel/core", "7.0.0"), "pkg:npm/%40babel/core@7.0.0");
    }

    #[test]
    fn sanitize_spdx_id_strips_unsafe() {
        assert_eq!(sanitize_spdx_id("@babel/core@7.0.0"), "-babel-core-7.0.0");
    }

    #[test]
    fn format_unix_utc_epoch() {
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_unix_utc_known_date() {
        // 2024-03-01T12:34:56Z = 1709296496
        assert_eq!(format_unix_utc(1709296496), "2024-03-01T12:34:56Z");
    }
}
