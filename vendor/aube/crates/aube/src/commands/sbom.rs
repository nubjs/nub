//! `aube sbom` — emit a Software Bill of Materials for the installed graph.
//!
//! Reads the lockfile and installed package manifests (no network, no linking),
//! walks the root importer's direct deps transitively, and serializes the
//! closure as either CycloneDX 1.5 JSON or SPDX 2.3 JSON. Pure read; does not
//! modify `node_modules/` or take the project lock.

use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use miette::{Context, IntoDiagnostic};
use std::collections::BTreeMap;

use super::DepFilter;

#[derive(Debug, usage_rs::Args)]
pub struct SbomArgs {
    /// Show only devDependencies
    #[usage(short = 'D', long, conflicts = "--prod")]
    pub dev: bool,

    /// Exclude peer dependencies from CycloneDX output
    #[usage(long)]
    pub exclude_peers: bool,

    /// Output format: `cyclonedx` (default) or `spdx`
    #[usage(long, value_enum, default_value_t = SbomFormat::Cyclonedx, default = "cyclonedx")]
    pub format: SbomFormat,

    /// Describe the complete platform-independent lockfile graph
    #[usage(long)]
    pub lockfile_only: bool,

    /// Show only production dependencies (skip devDependencies)
    #[usage(short = 'P', long, long = "production", conflicts = "--dev")]
    pub prod: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, usage_rs::ValueEnum, strum::Display, strum::EnumString,
)]
#[strum(serialize_all = "kebab-case")]
pub enum SbomFormat {
    Cyclonedx,
    Spdx,
}

pub async fn run(args: SbomArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let mut graph = super::load_graph(
        &cwd,
        &manifest,
        &format!(
            "no lockfile found — run `{}` before generating an SBOM",
            aube_util::cmd("install")
        ),
    )?;

    // Lockfiles intentionally retain optional native packages for every
    // platform. A normal SBOM describes what this host can install, while
    // --lockfile-only preserves the complete platform-independent graph.
    if !args.lockfile_only {
        let workspace = aube_manifest::WorkspaceConfig::load(&cwd).map_err(|err| {
            miette::miette!(
                code = aube_codes::errors::ERR_AUBE_WORKSPACE_PARSE,
                "failed to load workspace config: {err}"
            )
        })?;
        let (os, cpu, libc) =
            aube_manifest::effective_supported_architectures(&manifest, &workspace);
        let supported = aube_resolver::SupportedArchitectures {
            os,
            cpu,
            libc,
            ..Default::default()
        };
        let ignored = aube_manifest::effective_ignored_optional_dependencies(&manifest, &workspace);
        aube_resolver::platform::filter_graph(&mut graph, &supported, &ignored);
    }

    let filter = DepFilter::from_flags(args.prod, args.dev);
    let closure = super::collect_dep_closure(&graph, filter, false);
    if args.exclude_peers && args.format != SbomFormat::Cyclonedx {
        return Err(miette::miette!(
            "--exclude-peers is only supported with --format cyclonedx"
        ));
    }
    let closure = if args.exclude_peers && args.format == SbomFormat::Cyclonedx {
        exclude_peer_only_packages(
            &graph,
            &closure,
            &manifest.peer_dependencies,
            &manifest.dependencies,
        )
    } else {
        closure
    };
    let installed_metadata = super::licenses::collect_installed_licenses(
        &cwd,
        &graph,
        closure.keys().map(String::as_str),
    )?;

    let json = match args.format {
        SbomFormat::Cyclonedx => {
            let dev_only = collect_dev_only_packages(&graph, graph.importers.values().flatten());
            render_cyclonedx(
                &manifest,
                &closure,
                &installed_metadata,
                &dev_only,
                args.exclude_peers,
            )?
        }
        SbomFormat::Spdx => render_spdx(&manifest, &graph, filter, &closure, &installed_metadata)?,
    };

    println!("{json}");
    Ok(())
}

/// CycloneDX 1.5 JSON. See https://cyclonedx.org/docs/1.5/json/.
fn render_cyclonedx(
    manifest: &aube_manifest::PackageJson,
    closure: &BTreeMap<String, &LockedPackage>,
    installed_metadata: &BTreeMap<String, super::licenses::InstalledPackageMetadata>,
    dev_only: &BTreeMap<String, bool>,
    exclude_peers: bool,
) -> miette::Result<String> {
    let root_name = manifest.name.clone().unwrap_or_else(|| "(unnamed)".into());
    let root_version = manifest.version.clone().unwrap_or_default();
    let root_ref = format!("{root_name}@{root_version}");

    let mut components = Vec::new();
    let mut license_cache = BTreeMap::new();
    for (dep_path, pkg) in closure {
        let mut c = serde_json::Map::new();
        c.insert("type".into(), "library".into());
        c.insert("bom-ref".into(), dep_path.clone().into());
        c.insert("name".into(), pkg.name.clone().into());
        c.insert("version".into(), pkg.version.clone().into());
        c.insert("purl".into(), purl(&pkg.name, &pkg.version).into());
        if let Some(license) = installed_metadata
            .get(dep_path)
            .and_then(|metadata| metadata.license.as_deref())
        {
            let licenses = license_cache
                .entry(license)
                .or_insert_with(|| cyclonedx_licenses(license))
                .clone();
            c.insert("licenses".into(), licenses);
        }
        if let Some(bugs_url) = bugs_url_from_extra(&pkg.extra_meta) {
            c.insert("externalReferences".into(), external_references(bugs_url));
        }
        if dev_only.get(dep_path).copied().unwrap_or(false) {
            c.insert("scope".into(), "excluded".into());
            c.insert(
                "properties".into(),
                serde_json::json!([{
                    "name": "cdx:npm:package:development",
                    "value": "true",
                }]),
            );
        }
        components.push(serde_json::Value::Object(c));
    }

    let mut root_component = serde_json::Map::new();
    root_component.insert("type".into(), "application".into());
    root_component.insert("bom-ref".into(), root_ref.clone().into());
    root_component.insert("name".into(), root_name.into());
    if !root_version.is_empty() {
        root_component.insert("version".into(), root_version.clone().into());
    }
    if let Some(license) = manifest_license(manifest) {
        root_component.insert("licenses".into(), cyclonedx_licenses(&license));
    }
    if let Some(bugs_url) = bugs_url_from_extra(&manifest.extra) {
        root_component.insert("externalReferences".into(), external_references(bugs_url));
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

    let mut bom = serde_json::Map::new();
    bom.insert("bomFormat".into(), "CycloneDX".into());
    bom.insert("specVersion".into(), "1.5".into());
    bom.insert("version".into(), 1.into());
    bom.insert("metadata".into(), metadata.into());
    bom.insert("components".into(), components.into());
    if exclude_peers {
        bom.insert("properties".into(), cyclonedx_exclude_peers_property());
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(bom))
        .into_diagnostic()
        .wrap_err("failed to serialize CycloneDX SBOM")
}

fn cyclonedx_exclude_peers_property() -> serde_json::Value {
    serde_json::json!([{
        "name": "cdx:npm:package:excludePeers",
        "value": "true",
    }])
}

fn cyclonedx_licenses(license: &str) -> serde_json::Value {
    if spdx::license_id(license).is_some() {
        serde_json::json!([{ "license": { "id": license } }])
    } else if spdx::Expression::parse(license).is_ok() {
        serde_json::json!([{ "expression": license }])
    } else {
        serde_json::json!([{ "license": { "name": license } }])
    }
}

fn manifest_license(manifest: &aube_manifest::PackageJson) -> Option<String> {
    super::licenses::license_from_values(
        manifest.extra.get("license"),
        manifest.extra.get("licenses"),
    )
}

fn external_references(url: &str) -> serde_json::Value {
    serde_json::json!([{
        "type": "issue-tracker",
        "url": url,
    }])
}

fn bugs_url_from_extra(extra: &BTreeMap<String, serde_json::Value>) -> Option<&str> {
    match extra.get("bugs")? {
        serde_json::Value::String(url) => url_is_http(url).then_some(url.as_str()),
        serde_json::Value::Object(map) => map
            .get("url")
            .and_then(serde_json::Value::as_str)
            .filter(|url| url_is_http(url)),
        _ => None,
    }
}

fn url_is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn exclude_peer_only_packages<'a>(
    graph: &'a LockfileGraph,
    closure: &BTreeMap<String, &'a LockedPackage>,
    root_peer_dependencies: &BTreeMap<String, String>,
    root_dependencies: &BTreeMap<String, String>,
) -> BTreeMap<String, &'a LockedPackage> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<String> = graph
        .root_deps()
        .iter()
        .filter(|dep| closure.contains_key(&dep.dep_path))
        .filter(|dep| {
            !root_peer_dependencies.contains_key(&dep.name)
                || root_dependencies.contains_key(&dep.name)
        })
        .map(|dep| dep.dep_path.clone())
        .collect();

    while let Some(dep_path) = stack.pop() {
        let Some(pkg) = closure.get(&dep_path).copied() else {
            continue;
        };
        if out.insert(dep_path.clone(), pkg).is_some() {
            continue;
        }
        let peer_names = pkg.peer_dependencies_with_meta_defaults();
        for (name, version) in &pkg.dependencies {
            if peer_names.contains_key(name) {
                continue;
            }
            let child_path = format!("{name}@{version}");
            if closure.contains_key(&child_path) {
                stack.push(child_path);
            }
        }
    }

    out
}

fn collect_dev_only_packages<'a>(
    graph: &LockfileGraph,
    roots: impl IntoIterator<Item = &'a DirectDep>,
) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<(String, bool)> = roots
        .into_iter()
        .map(|d| (d.dep_path.clone(), matches!(d.dep_type, DepType::Dev)))
        .collect();

    while let Some((dep_path, dev_only)) = stack.pop() {
        match out.get(&dep_path).copied() {
            Some(false) => continue,
            Some(true) if dev_only => continue,
            _ => {
                out.insert(dep_path.clone(), dev_only);
            }
        }

        let Some(pkg) = graph.get_package(&dep_path) else {
            continue;
        };
        for (name, version) in &pkg.dependencies {
            stack.push((format!("{name}@{version}"), dev_only));
        }
    }

    out
}

/// SPDX 2.3 JSON. See https://spdx.github.io/spdx-spec/v2.3/.
fn render_spdx(
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    filter: DepFilter,
    closure: &BTreeMap<String, &LockedPackage>,
    installed_metadata: &BTreeMap<String, super::licenses::InstalledPackageMetadata>,
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
    root_pkg.insert("licenseConcluded".into(), "NOASSERTION".into());
    root_pkg.insert(
        "licenseDeclared".into(),
        spdx_declared_license(manifest_license(manifest).as_deref()).into(),
    );
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
        p.insert(
            "licenseDeclared".into(),
            spdx_declared_license(
                installed_metadata
                    .get(dep_path)
                    .and_then(|metadata| metadata.license.as_deref()),
            )
            .into(),
        );
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
        "https://aube.sh/spdx/{}-{}-{}.{:09}",
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

fn spdx_declared_license(license: Option<&str>) -> &str {
    license
        .filter(|license| spdx::Expression::parse(license).is_ok())
        .unwrap_or("NOASSERTION")
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
    fn cyclonedx_licenses_distinguishes_ids_expressions_and_names() {
        assert_eq!(
            cyclonedx_licenses("MIT"),
            serde_json::json!([{ "license": { "id": "MIT" } }])
        );
        assert_eq!(
            cyclonedx_licenses("MIT OR Apache-2.0"),
            serde_json::json!([{ "expression": "MIT OR Apache-2.0" }])
        );
        assert_eq!(
            cyclonedx_licenses("SEE LICENSE IN LICENSE.md"),
            serde_json::json!([{
                "license": { "name": "SEE LICENSE IN LICENSE.md" }
            }])
        );
    }

    #[test]
    fn spdx_declared_license_rejects_non_spdx_names() {
        assert_eq!(
            spdx_declared_license(Some("MIT OR Apache-2.0")),
            "MIT OR Apache-2.0"
        );
        assert_eq!(
            spdx_declared_license(Some("SEE LICENSE IN LICENSE.md")),
            "NOASSERTION"
        );
        assert_eq!(spdx_declared_license(None), "NOASSERTION");
    }

    #[test]
    fn collect_dev_only_packages_handles_cycles_and_runtime_downgrade() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "a@1.0.0".into(),
            LockedPackage {
                name: "a".into(),
                version: "1.0.0".into(),
                dep_path: "a@1.0.0".into(),
                dependencies: BTreeMap::from([("b".into(), "1.0.0".into())]),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "b@1.0.0".into(),
            LockedPackage {
                name: "b".into(),
                version: "1.0.0".into(),
                dep_path: "b@1.0.0".into(),
                dependencies: BTreeMap::from([("a".into(), "1.0.0".into())]),
                ..Default::default()
            },
        );

        let roots = vec![DirectDep {
            name: "a".into(),
            dep_path: "a@1.0.0".into(),
            dep_type: DepType::Dev,
            specifier: None,
        }];
        let dev_only = collect_dev_only_packages(&graph, &roots);
        assert_eq!(dev_only.get("a@1.0.0"), Some(&true));
        assert_eq!(dev_only.get("b@1.0.0"), Some(&true));

        let roots = vec![
            DirectDep {
                name: "a".into(),
                dep_path: "a@1.0.0".into(),
                dep_type: DepType::Dev,
                specifier: None,
            },
            DirectDep {
                name: "b".into(),
                dep_path: "b@1.0.0".into(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ];
        let runtime = collect_dev_only_packages(&graph, &roots);
        assert_eq!(runtime.get("a@1.0.0"), Some(&false));
        assert_eq!(runtime.get("b@1.0.0"), Some(&false));
    }

    #[test]
    fn collect_dev_only_packages_uses_all_workspace_importers() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "shared@1.0.0".into(),
            LockedPackage {
                name: "shared".into(),
                version: "1.0.0".into(),
                dep_path: "shared@1.0.0".into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "shared".into(),
                dep_path: "shared@1.0.0".into(),
                dep_type: DepType::Dev,
                specifier: None,
            }],
        );
        graph.importers.insert(
            "packages/app".into(),
            vec![DirectDep {
                name: "shared".into(),
                dep_path: "shared@1.0.0".into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let dev_only = collect_dev_only_packages(&graph, graph.importers.values().flatten());
        assert_eq!(dev_only.get("shared@1.0.0"), Some(&false));
    }

    #[test]
    fn exclude_peer_only_packages_prunes_exclusive_peer_subtree() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "consumer".into(),
                dep_path: "consumer@1.0.0".into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );
        graph.packages.insert(
            "consumer@1.0.0".into(),
            LockedPackage {
                name: "consumer".into(),
                version: "1.0.0".into(),
                dep_path: "consumer@1.0.0".into(),
                dependencies: BTreeMap::from([
                    ("peer".into(), "1.0.0".into()),
                    ("runtime".into(), "1.0.0".into()),
                ]),
                peer_dependencies: BTreeMap::from([("peer".into(), "^1".into())]),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "peer@1.0.0".into(),
            LockedPackage {
                name: "peer".into(),
                version: "1.0.0".into(),
                dep_path: "peer@1.0.0".into(),
                dependencies: BTreeMap::from([("peer-child".into(), "1.0.0".into())]),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "peer-child@1.0.0".into(),
            LockedPackage {
                name: "peer-child".into(),
                version: "1.0.0".into(),
                dep_path: "peer-child@1.0.0".into(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "runtime@1.0.0".into(),
            LockedPackage {
                name: "runtime".into(),
                version: "1.0.0".into(),
                dep_path: "runtime@1.0.0".into(),
                dependencies: BTreeMap::from([("peer-child".into(), "1.0.0".into())]),
                ..Default::default()
            },
        );

        let closure: BTreeMap<_, _> = graph
            .packages
            .iter()
            .map(|(dep_path, pkg)| (dep_path.clone(), pkg))
            .collect();
        let pruned =
            exclude_peer_only_packages(&graph, &closure, &BTreeMap::new(), &BTreeMap::new());

        assert!(pruned.contains_key("consumer@1.0.0"));
        assert!(pruned.contains_key("runtime@1.0.0"));
        assert!(pruned.contains_key("peer-child@1.0.0"));
        assert!(!pruned.contains_key("peer@1.0.0"));
    }

    #[test]
    fn exclude_peer_only_packages_prunes_root_peer_deps() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "react".into(),
                dep_path: "react@18.2.0".into(),
                dep_type: DepType::Dev,
                specifier: None,
            }],
        );
        graph.packages.insert(
            "react@18.2.0".into(),
            LockedPackage {
                name: "react".into(),
                version: "18.2.0".into(),
                dep_path: "react@18.2.0".into(),
                ..Default::default()
            },
        );
        let closure: BTreeMap<_, _> = graph
            .packages
            .iter()
            .map(|(dep_path, pkg)| (dep_path.clone(), pkg))
            .collect();
        let pruned = exclude_peer_only_packages(
            &graph,
            &closure,
            &BTreeMap::from([("react".into(), "^18".into())]),
            &BTreeMap::new(),
        );

        assert!(pruned.is_empty());
    }

    #[test]
    fn exclude_peer_only_packages_keeps_root_prod_peer_deps() {
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "react".into(),
                dep_path: "react@18.2.0".into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );
        graph.packages.insert(
            "react@18.2.0".into(),
            LockedPackage {
                name: "react".into(),
                version: "18.2.0".into(),
                dep_path: "react@18.2.0".into(),
                ..Default::default()
            },
        );
        let closure: BTreeMap<_, _> = graph
            .packages
            .iter()
            .map(|(dep_path, pkg)| (dep_path.clone(), pkg))
            .collect();
        let pruned = exclude_peer_only_packages(
            &graph,
            &closure,
            &BTreeMap::from([("react".into(), "^18".into())]),
            &BTreeMap::from([("react".into(), "^18".into())]),
        );

        assert!(pruned.contains_key("react@18.2.0"));
    }

    #[test]
    fn bugs_url_from_extra_accepts_url_shape_only() {
        let object = BTreeMap::from([(
            "bugs".into(),
            serde_json::json!({"url": "https://github.com/acme/pkg/issues"}),
        )]);
        assert_eq!(
            bugs_url_from_extra(&object),
            Some("https://github.com/acme/pkg/issues")
        );

        let email = BTreeMap::from([(
            "bugs".into(),
            serde_json::json!({"email": "bugs@acme.test"}),
        )]);
        assert_eq!(bugs_url_from_extra(&email), None);
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
