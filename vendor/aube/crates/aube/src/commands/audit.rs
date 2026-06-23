//! `aube audit` — check installed packages against the registry advisory DB.
//!
//! Walks the lockfile's resolved package set (filtered by `--prod`/`--dev`),
//! posts `{name: [versions]}` to the registry's
//! `/-/npm/v1/security/advisories/bulk` endpoint, and prints the matching
//! advisories. Mirrors `pnpm audit`'s default table layout, and its
//! `--json` shape: `{ "advisories": { "<id>": {advisory…}, … },
//! "metadata": { "vulnerabilities": {info,low,moderate,high,critical},
//! dependencies, devDependencies, optionalDependencies, totalDependencies
//! } }`. The `advisories` map is keyed by the numeric npm advisory `id`
//! (as a string), so a single advisory affecting several packages
//! collapses to one entry; `metadata.vulnerabilities` counts one per
//! unique advisory.
//!
//! Read-only unless `--fix` is passed.

use super::DepFilter;
use aube_registry::Packument;
use aube_registry::client::RegistryClient;
use aube_registry::config::normalize_registry_url_pub;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube audit
  Severity  Package    Vulnerable  Title
  moderate  minimatch  <3.0.5      Regular Expression Denial of Service
                                   https://github.com/advisories/GHSA-f8q6-p94x

  1 vulnerability found

  # Only fail on high and above
  $ aube audit --audit-level high

  # Skip optional deps and dev deps
  $ aube audit --prod --no-optional

  # Pipe into jq
  $ aube audit --json | jq '.advisories | length'

  # Clean
  $ aube audit
  No known vulnerabilities found
";

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Only print advisories at or above this severity.
    ///
    /// One of: `info`, `low`, `moderate`, `high`, `critical`. Default: `low`.
    #[arg(long, value_enum, default_value_t = Severity::Low)]
    pub audit_level: Severity,

    /// Only audit `devDependencies`.
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Fix advisories.
    ///
    /// Bare `--fix` writes package.json overrides for backwards compatibility.
    /// `--fix=update` refreshes the lockfile without writing overrides.
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "override")]
    pub fix: Option<FixMode>,

    /// Drop advisories whose ID matches one of these values.
    ///
    /// Matches against the numeric npm advisory `id`,
    /// `github_advisory_id` (`GHSA-…`), and any entry in `cves[]`
    /// (case-insensitive). Repeatable; comma-separated values are also
    /// accepted.
    #[arg(long, value_name = "ID", value_delimiter = ',')]
    pub ignore: Vec<String>,

    /// Use exit code 0 if the registry responds with an error.
    ///
    /// Useful when audit checks run in CI and the registry has a hiccup.
    #[arg(long)]
    pub ignore_registry_errors: bool,

    /// Drop advisories that have no non-vulnerable upgrade.
    ///
    /// Filters out advisories for which no non-vulnerable version is
    /// available in the package's packument. Same "best non-vulnerable"
    /// logic as `--fix`: an advisory is kept only when an upgrade path
    /// exists.
    #[arg(long)]
    pub ignore_unfixable: bool,

    /// Pick which advisories to fix interactively.
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Emit the report as JSON (pnpm-compatible shape) instead of a table.
    #[arg(long)]
    pub json: bool,

    /// Skip `optionalDependencies`.
    #[arg(long)]
    pub no_optional: bool,

    /// Only audit `dependencies` and `optionalDependencies`.
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    clap::ValueEnum,
    strum::Display,
    strum::EnumString,
)]
#[value(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum Severity {
    // Declaration order IS severity order (`Ord` is derived): `Info` is
    // the lowest, matching pnpm's `AUDIT_LEVEL_NUMBER` (`info: 0`). npm's
    // bulk advisory endpoint can return `severity: "info"` advisories, so
    // the variant must exist for them to count in `metadata` AND appear
    // in the `advisories` map under `--audit-level info`.
    Info,
    Low,
    Moderate,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum FixMode {
    /// Refresh the lockfile to patched versions allowed by existing ranges.
    Update,
    /// Write package.json overrides that force patched versions.
    Override,
}

pub async fn run(args: AuditArgs) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root()?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let graph = super::load_graph(
        &cwd,
        &manifest,
        &format!(
            "no lockfile found — run `{}` before `{}`",
            aube_util::cmd("install"),
            aube_util::cmd("audit")
        ),
    )?;

    let filter = DepFilter::from_flags(args.prod, args.dev);
    let closure = super::collect_dep_closure(&graph, filter, args.no_optional);

    // Build the bulk request body: { name: [version, ...] } with versions
    // deduped so the registry doesn't do extra work on a diamond dep.
    let mut pkg_versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pkg in closure.values() {
        let entry = pkg_versions.entry(pkg.name.clone()).or_default();
        if !entry.contains(&pkg.version) {
            entry.push(pkg.version.clone());
        }
    }

    if pkg_versions.is_empty() {
        if args.json {
            // Same well-formed shape consumers get on a non-empty run,
            // so `.advisories` / `.metadata` are always present.
            let report = build_audit_report(
                &serde_json::json!({}),
                args.audit_level,
                count_dependencies(&graph, filter, args.no_optional, &closure),
            );
            let out = serde_json::to_string_pretty(&report).into_diagnostic()?;
            println!("{out}");
        } else {
            println!("No dependencies to audit.");
        }
        return Ok(None);
    }

    let client = build_client(&cwd, args.network.registry.as_deref());
    let raw = match client.fetch_advisories_bulk(&pkg_versions).await {
        Ok(v) => v,
        Err(e) => {
            if args.ignore_registry_errors {
                // Old code here printed "No known vulnerabilities
                // found" and exited 0. That is a false negative. User
                // passed --ignore-registry-errors to keep CI green
                // when offline, but silently claiming clean audit can
                // mask real CVEs. If the registry was down on a scan
                // day, whole pipeline would ship vuln'd code with a
                // "passed" audit step. Now: report degraded status,
                // exit non-zero. CI fails loudly. JSON consumers
                // check `status` field.
                eprintln!("warn: advisory fetch failed: {e}");
                if args.json {
                    // Build the JSON via serde_json so the error
                    // message gets properly escaped. Hand-rolled
                    // string interpolation broke on errors that
                    // contained `"` (connection refused sometimes
                    // surfaces quoted URL text), producing
                    // malformed JSON that downstream consumers
                    // could not parse.
                    let reason = format!("advisory fetch failed: {e}");
                    let body = serde_json::json!({
                        "status": "degraded",
                        "reason": reason,
                    });
                    println!("{body}");
                } else {
                    eprintln!(
                        "audit degraded: advisory fetch failed, vulnerability status unknown"
                    );
                }
                // Degraded status exits 2. Return the code for the binary's
                // single `std::process::exit` rather than terminating here,
                // keeping the command embed-safe.
                return Ok(Some(2));
            }
            return Err(miette!("advisory fetch failed: {e}"));
        }
    };

    let ignored_ids = configured_audit_ignores(&manifest, &args.ignore);

    // `--ignore` / auditConfig ignores are cheap JSON filters. Run
    // them first so we don't bother fetching packuments for advisories
    // the user already said to drop.
    let raw = if ignored_ids.is_empty() {
        raw
    } else {
        filter_ignored_ids(&raw, &ignored_ids)
    };

    // `--ignore-unfixable` is expensive (one packument fetch per
    // vulnerable package), so table output scopes those fetches to the
    // displayed threshold. JSON metadata counts advisories before the
    // level filter, so JSON filtering must consider every known severity.
    let raw = if args.ignore_unfixable {
        filter_unfixable(
            &raw,
            &client,
            unfixable_filter_threshold(args.json, args.audit_level),
        )
        .await?
    } else {
        raw
    };

    let rows = flatten_advisories(&raw, args.audit_level);

    if args.interactive && args.json {
        return Err(miette!("--interactive cannot be used with --json"));
    }

    let fix_mode = args.fix.or(args.interactive.then_some(FixMode::Override));
    if let Some(mode) = fix_mode
        && !rows.is_empty()
    {
        let selected = if args.interactive {
            match select_fix_rows(&rows)? {
                Some(sel) => sel,
                // Picker cancelled (Ctrl-C / Esc): exit 130 via the return path.
                None => return Ok(Some(130)),
            }
        } else {
            rows.clone()
        };
        match mode {
            FixMode::Update => {
                let remaining = write_fix_lockfile_update(
                    &cwd,
                    &manifest,
                    &graph,
                    filter,
                    args.no_optional,
                    &selected,
                )
                .await?;
                if remaining.is_empty() {
                    return Ok(None);
                }
                render_fix_remaining(&selected, &remaining);
                // Some advisories remain unfixed: exit 1. Return the code for
                // the binary's single `std::process::exit` rather than
                // terminating here, keeping the command embed-safe.
                return Ok(Some(1));
            }
            FixMode::Override => {
                write_fix_overrides(&cwd, &selected, &client).await?;
                return Ok(None);
            }
        }
    }

    if args.json {
        // pnpm audit --json shape: `{ advisories: { "<id>": {…} },
        // metadata: { vulnerabilities, dependencies, … } }`. Built from
        // the post-ignore `raw`; `metadata.vulnerabilities` counts every
        // (post-ignore) advisory, while the `advisories` map is
        // additionally level-filtered.
        let report = build_audit_report(
            &raw,
            args.audit_level,
            count_dependencies(&graph, filter, args.no_optional, &closure),
        );
        let out = serde_json::to_string_pretty(&report).into_diagnostic()?;
        println!("{out}");
    } else {
        render_table(&rows);
    }

    if rows.is_empty() {
        Ok(None)
    } else {
        // pnpm-compat: exit 1 when any advisory matches the threshold. Return
        // the code for the binary's single `std::process::exit` rather than
        // terminating here, keeping the command embed-safe.
        Ok(Some(1))
    }
}

/// Interactively pick which advisories to fix. `Ok(Some(rows))` is the
/// selection (all rows when not a TTY); `Ok(None)` means the user cancelled
/// the picker (Ctrl-C / Esc), which the caller maps to exit code 130 —
/// returned up to the binary's single `std::process::exit` rather than
/// terminating here, keeping the command embed-safe.
fn select_fix_rows(rows: &[Row]) -> miette::Result<Option<Vec<Row>>> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(Some(rows.to_vec()));
    }

    let picked = match advisory_picker(rows).run() {
        Ok(picked) => picked,
        // Cancelled: signal to the caller, which returns exit code 130.
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
        Err(e) => {
            return Err(e)
                .into_diagnostic()
                .wrap_err("failed to read audit selection");
        }
    };
    Ok(Some(
        picked.into_iter().map(|idx| rows[idx].clone()).collect(),
    ))
}

fn advisory_picker(rows: &[Row]) -> demand::MultiSelect<'_, usize> {
    let mut picker = demand::MultiSelect::new("Choose which vulnerabilities to fix")
        .description("Space to toggle, Enter to confirm")
        .filterable(true)
        .min(1);
    for (idx, row) in rows.iter().enumerate() {
        let label = format!(
            "{} {} {} {}",
            row.severity, row.name, row.vulnerable_versions, row.title
        );
        let mut option = demand::DemandOption::new(idx).label(&label).selected(true);
        if !row.url.is_empty() {
            option = option.description(&row.url);
        }
        picker = picker.option(option);
    }
    picker
}

fn render_fix_remaining(selected: &[Row], remaining: &[Row]) {
    let fixed = selected.len().saturating_sub(remaining.len());
    eprintln!(
        "{} fixed, {} remain.",
        pluralizer::pluralize("vulnerability", fixed as isize, true),
        remaining.len()
    );
    if !remaining.is_empty() {
        eprintln!();
        eprintln!("Remaining vulnerabilities:");
        for row in remaining {
            eprintln!("- ({}) \"{}\" {}", row.severity, row.title, row.name);
        }
    }
}

/// Construct the registry client, optionally pointing the default
/// registry (used for the bulk advisory POST and non-scoped packument
/// fetches) at `registry_override`. Scoped registries from `.npmrc`
/// / `aube-workspace.yaml` still win for their own packument lookups
/// — they exist precisely because advisory tooling can't know which
/// mirror a scope is pinned to.
fn build_client(cwd: &std::path::Path, registry_override: Option<&str>) -> RegistryClient {
    let mut config = super::load_npm_config(cwd);
    if let Some(url) = registry_override {
        config.registry = normalize_registry_url_pub(url);
    }
    tracing::debug!("registry: {}", config.registry);
    for (scope, url) in &config.scoped_registries {
        tracing::debug!("scoped registry: {scope} -> {url}");
    }
    let policy = super::resolve_fetch_policy(cwd);
    RegistryClient::from_config_with_policy(config, policy)
}

/// Drop advisories whose ID matches any `ignore` value. Matches
/// against the npm numeric `id`, the `github_advisory_id`, and each
/// entry in `cves[]`. IDs are compared case-insensitively as strings
/// so users can pass either `GHSA-abcd-...` or the same in uppercase
/// / lowercase, or the CVE form. Packages whose advisories all get
/// filtered out drop from the response entirely.
fn configured_audit_ignores(manifest: &aube_manifest::PackageJson, cli: &[String]) -> Vec<String> {
    let mut out = cli.to_vec();
    let Some(config) = manifest
        .extra
        .get("auditConfig")
        .and_then(|v| v.as_object())
    else {
        return out;
    };
    for key in ["ignoreGhsas", "ignoreCves"] {
        if let Some(values) = config.get(key).and_then(|v| v.as_array()) {
            out.extend(values.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
    }
    out
}

fn filter_ignored_ids(v: &serde_json::Value, ignore: &[String]) -> serde_json::Value {
    use serde_json::{Map, Value};
    let Some(obj) = v.as_object() else {
        return v.clone();
    };
    let needles: BTreeSet<String> = ignore.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut out: Map<String, Value> = Map::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        let kept: Vec<Value> = arr
            .iter()
            .filter(|adv| !advisory_matches_ignore(adv, &needles))
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(name.clone(), Value::Array(kept));
        }
    }
    Value::Object(out)
}

fn advisory_matches_ignore(adv: &serde_json::Value, needles: &BTreeSet<String>) -> bool {
    if let Some(id) = adv.get("id") {
        let id_str = match id {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        if !id_str.is_empty() && needles.contains(&id_str.to_ascii_lowercase()) {
            return true;
        }
    }
    if let Some(ghsa) = adv.get("github_advisory_id").and_then(|v| v.as_str())
        && needles.contains(&ghsa.to_ascii_lowercase())
    {
        return true;
    }
    if let Some(cves) = adv.get("cves").and_then(|v| v.as_array()) {
        for cve in cves {
            if let Some(s) = cve.as_str()
                && needles.contains(&s.to_ascii_lowercase())
            {
                return true;
            }
        }
    }
    false
}

fn unfixable_filter_threshold(json: bool, audit_level: Severity) -> Severity {
    if json { Severity::Info } else { audit_level }
}

/// Drop advisories whose `vulnerable_versions` range cannot be
/// escaped: we ask `best_non_vulnerable` whether the packument has a
/// clean version outside the range, and when the answer is "no" the
/// advisory is considered unfixable and filtered out. Any packument
/// fetch error leaves the advisories untouched — being wrong about
/// "unfixable" is worse than a harmless overreport, matching the
/// spirit of `--ignore-registry-errors`.
async fn filter_unfixable(
    v: &serde_json::Value,
    client: &RegistryClient,
    threshold: Severity,
) -> miette::Result<serde_json::Value> {
    use serde_json::{Map, Value};
    let Some(obj) = v.as_object() else {
        return Ok(v.clone());
    };
    let cache_dir = super::packument_cache_dir();
    let mut out: Map<String, Value> = Map::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        // Skip the packument fetch if every advisory on this package
        // is below the user's severity threshold — they'd all be
        // dropped by the level filter in `flatten_advisories` /
        // `build_audit_report` anyway.
        let has_in_threshold = arr.iter().any(|adv| {
            adv.get("severity")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse::<Severity>().ok())
                .is_some_and(|s| s >= threshold)
        });
        if !has_in_threshold {
            out.insert(name.clone(), Value::Array(arr.clone()));
            continue;
        }
        let packument = match client.fetch_packument_cached(name, &cache_dir).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_AUDIT_FETCH_FAILED,
                    "audit --ignore-unfixable: keeping advisories for {name}: packument fetch failed: {e}"
                );
                out.insert(name.clone(), Value::Array(arr.clone()));
                continue;
            }
        };
        let kept: Vec<Value> = arr
            .iter()
            .filter(|adv| {
                let Some(range) = adv.get("vulnerable_versions").and_then(|s| s.as_str()) else {
                    return true;
                };
                best_non_vulnerable(&packument, &[range.to_string()]).is_some()
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(name.clone(), Value::Array(kept));
        }
    }
    Ok(Value::Object(out))
}

async fn write_fix_overrides(
    cwd: &std::path::Path,
    rows: &[Row],
    client: &RegistryClient,
) -> miette::Result<()> {
    let manifest_path = cwd.join("package.json");
    let content = std::fs::read_to_string(&manifest_path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;
    let mut root = aube_manifest::parse_json::<serde_json::Value>(&manifest_path, content)
        .map_err(miette::Report::new)
        .wrap_err("failed to parse package.json")?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| miette!("package.json root must be an object"))?;

    let cache_dir = super::packument_cache_dir();
    let mut vulnerable_ranges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        vulnerable_ranges
            .entry(row.name.clone())
            .or_default()
            .push(row.vulnerable_versions.clone());
    }

    let mut fixes = BTreeMap::new();
    for (name, ranges) in vulnerable_ranges {
        let packument = client
            .fetch_packument_cached(&name, &cache_dir)
            .await
            .map_err(|e| miette!("failed to fetch packument for {name}: {e}"))?;
        if let Some(version) = best_non_vulnerable(&packument, &ranges) {
            fixes.insert(name.clone(), version);
        } else {
            eprintln!("warn: no patched version found for {name}");
        }
    }

    if fixes.is_empty() {
        eprintln!("No audit fixes available.");
        return Ok(());
    }

    let overrides = obj
        .entry("overrides".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let overrides = overrides
        .as_object_mut()
        .ok_or_else(|| miette!("package.json `overrides` must be an object for audit --fix"))?;
    for (name, version) in &fixes {
        overrides.insert(name.clone(), serde_json::Value::String(version.clone()));
    }

    super::write_manifest_json(&manifest_path, &root)?;
    eprintln!(
        "Updated package.json overrides for {} package(s).",
        fixes.len()
    );
    Ok(())
}

async fn write_fix_lockfile_update(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    graph: &aube_lockfile::LockfileGraph,
    filter: DepFilter,
    no_optional: bool,
    rows: &[Row],
) -> miette::Result<Vec<Row>> {
    let vulnerable_ranges = vulnerable_ranges_by_name(rows);
    let vulnerable_names: BTreeSet<String> = vulnerable_ranges.keys().cloned().collect();
    let before = resolved_versions_by_name(graph, filter, no_optional, &vulnerable_names);
    let mut resolver_manifest = manifest.clone();
    widen_vulnerable_direct_pins(&mut resolver_manifest, graph, &vulnerable_ranges);

    let workspace_catalogs = super::load_workspace_catalogs(cwd)?;
    let mut resolver = super::build_resolver(cwd, &resolver_manifest, workspace_catalogs)?
        .with_vulnerable_ranges(vulnerable_ranges.clone());
    let mut new_graph = resolver
        .resolve(&resolver_manifest, Some(graph))
        .await
        .map_err(miette::Report::new)
        .wrap_err("failed to resolve audit fixes")?;

    let after = resolved_versions_by_name(&new_graph, filter, no_optional, &vulnerable_names);
    let remaining: Vec<Row> = rows
        .iter()
        .filter(|row| {
            after.get(&row.name).is_some_and(|versions| {
                versions.iter().any(|version| {
                    version_is_vulnerable(version, std::slice::from_ref(&row.vulnerable_versions))
                })
            })
        })
        .cloned()
        .collect();
    let updated = updated_package_names(rows, &before, &after);

    if updated.is_empty() {
        eprintln!("No audit lockfile updates available.");
        return Ok(remaining);
    }

    let mut output_manifest = manifest.clone();
    if update_direct_manifest_specs(&mut output_manifest, graph, &new_graph, &updated) {
        let manifest_path = cwd.join("package.json");
        super::write_manifest_dep_sections(&manifest_path, &output_manifest)?;
        eprintln!("Updated package.json");
    }

    sync_root_dep_specifiers(&mut new_graph, &output_manifest);
    crate::commands::install::finalize_lockfile_graph(
        cwd,
        &mut new_graph,
        &output_manifest,
        false,
        None,
    )
    .await?;
    super::write_and_log_lockfile(cwd, &new_graph, &output_manifest)?;
    for name in &updated {
        let old = before
            .get(name)
            .map(|v| v.iter().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "(not locked)".to_string());
        let new = after
            .get(name)
            .map(|v| v.iter().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "(removed)".to_string());
        eprintln!("  {name}: {old} -> {new}");
    }
    eprintln!("Updated lockfile for {} package(s).", updated.len());
    Ok(remaining)
}

fn vulnerable_ranges_by_name(rows: &[Row]) -> BTreeMap<String, Vec<String>> {
    let mut vulnerable_ranges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        vulnerable_ranges
            .entry(row.name.clone())
            .or_default()
            .push(row.vulnerable_versions.clone());
    }
    vulnerable_ranges
}

fn updated_package_names(
    selected: &[Row],
    before: &BTreeMap<String, BTreeSet<String>>,
    after: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    selected
        .iter()
        .filter(|row| before.get(&row.name) != after.get(&row.name))
        .map(|row| row.name.clone())
        .collect()
}

fn widen_vulnerable_direct_pins(
    manifest: &mut aube_manifest::PackageJson,
    graph: &aube_lockfile::LockfileGraph,
    vulnerable_ranges: &BTreeMap<String, Vec<String>>,
) {
    let direct_versions = direct_versions_by_name(graph);
    for (key, spec) in mutable_manifest_dep_entries(manifest) {
        let real_name = real_name_for_spec(&key, spec);
        let Some(version) = direct_versions.get(&real_name) else {
            continue;
        };
        let Some(ranges) = vulnerable_ranges.get(&real_name) else {
            continue;
        };
        if !version_is_vulnerable(version, ranges) || !looks_like_exact_version(spec_range(spec)) {
            continue;
        }
        *spec = rewrite_specifier(spec, &real_name, version, Some(">="));
    }
}

fn update_direct_manifest_specs(
    manifest: &mut aube_manifest::PackageJson,
    old_graph: &aube_lockfile::LockfileGraph,
    new_graph: &aube_lockfile::LockfileGraph,
    updated_names: &BTreeSet<String>,
) -> bool {
    let before = direct_versions_by_name(old_graph);
    let after = direct_versions_by_name(new_graph);
    let mut wrote_any = false;
    for (key, spec) in mutable_manifest_dep_entries(manifest) {
        let real_name = real_name_for_spec(&key, spec);
        if !updated_names.contains(&real_name) {
            continue;
        }
        let (Some(old), Some(new)) = (before.get(&real_name), after.get(&real_name)) else {
            continue;
        };
        if old == new {
            continue;
        }
        if spec_satisfies_version(spec, new) {
            continue;
        }
        let next = rewrite_specifier(spec, &real_name, new, None);
        if next != *spec {
            *spec = next;
            wrote_any = true;
        }
    }
    wrote_any
}

fn sync_root_dep_specifiers(
    graph: &mut aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) {
    let Some(deps) = graph.importers.get_mut(".") else {
        return;
    };
    for dep in deps {
        if let Some(spec) = manifest_spec_for_dep(manifest, dep) {
            dep.specifier = Some(spec);
        }
    }
}

fn manifest_spec_for_dep(
    manifest: &aube_manifest::PackageJson,
    dep: &aube_lockfile::DirectDep,
) -> Option<String> {
    let section = match dep.dep_type {
        aube_lockfile::DepType::Production => &manifest.dependencies,
        aube_lockfile::DepType::Dev => &manifest.dev_dependencies,
        aube_lockfile::DepType::Optional => &manifest.optional_dependencies,
    };
    section.get(&dep.name).cloned().or_else(|| {
        section
            .iter()
            .find(|(key, spec)| real_name_for_spec(key, spec) == dep.name)
            .map(|(_, spec)| spec.clone())
    })
}

fn direct_versions_by_name(graph: &aube_lockfile::LockfileGraph) -> BTreeMap<String, String> {
    graph
        .root_deps()
        .iter()
        .filter_map(|dep| {
            graph
                .get_package(&dep.dep_path)
                .map(|pkg| (pkg.registry_name().to_string(), pkg.version.clone()))
        })
        .collect()
}

fn mutable_manifest_dep_entries(
    manifest: &mut aube_manifest::PackageJson,
) -> impl Iterator<Item = (String, &mut String)> {
    manifest
        .dependencies
        .iter_mut()
        .chain(manifest.dev_dependencies.iter_mut())
        .chain(manifest.optional_dependencies.iter_mut())
        .map(|(key, spec)| (key.clone(), spec))
}

fn real_name_for_spec(manifest_key: &str, spec: &str) -> String {
    if let Some(rest) = spec.strip_prefix("npm:") {
        if let Some(at_idx) = rest.rfind('@') {
            return rest[..at_idx].to_string();
        }
        return rest.to_string();
    }
    manifest_key.to_string()
}

fn rewrite_specifier(
    original: &str,
    real_name: &str,
    resolved_version: &str,
    force_prefix: Option<&str>,
) -> String {
    let (range, is_alias) = if let Some(rest) = original.strip_prefix("npm:") {
        (rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(""), true)
    } else {
        (original, false)
    };
    let prefix = force_prefix.unwrap_or_else(|| rewrite_prefix(range));
    let versioned = format!("{prefix}{resolved_version}");
    if is_alias {
        format!("npm:{real_name}@{versioned}")
    } else {
        versioned
    }
}

fn rewrite_prefix(spec: &str) -> &'static str {
    if is_compound_range(spec) {
        ">="
    } else {
        range_prefix(spec)
    }
}

fn is_compound_range(spec: &str) -> bool {
    spec.contains("||") || spec.split_whitespace().nth(1).is_some()
}

fn spec_range(spec: &str) -> &str {
    if let Some(rest) = spec.strip_prefix("npm:") {
        rest.rsplit_once('@').map(|(_, range)| range).unwrap_or("")
    } else {
        spec
    }
}

fn spec_satisfies_version(spec: &str, version: &str) -> bool {
    let Ok(version) = node_semver::Version::parse(version) else {
        return false;
    };
    node_semver::Range::parse(spec_range(spec))
        .ok()
        .is_some_and(|range| version.satisfies(&range))
}

fn range_prefix(spec: &str) -> &'static str {
    let trimmed = spec.trim_start();
    if trimmed.starts_with('^') {
        "^"
    } else if trimmed.starts_with('~') {
        "~"
    } else if trimmed.starts_with(">=") {
        ">="
    } else if trimmed.starts_with("<=") {
        "<="
    } else if trimmed.starts_with('>') {
        ">"
    } else if trimmed.starts_with('<') {
        "<"
    } else if trimmed.starts_with('=') {
        "="
    } else {
        ""
    }
}

fn looks_like_exact_version(spec: &str) -> bool {
    let mut chars = spec.trim_start().chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

fn resolved_versions_by_name(
    graph: &aube_lockfile::LockfileGraph,
    filter: DepFilter,
    no_optional: bool,
    names: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<String>> {
    let closure = super::collect_dep_closure(graph, filter, no_optional);
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for pkg in closure.values() {
        let name = pkg.registry_name();
        if names.contains(name) {
            out.entry(name.to_string())
                .or_default()
                .insert(pkg.version.clone());
        }
    }
    out
}

fn version_is_vulnerable(version: &str, vulnerable_versions: &[String]) -> bool {
    let Ok(version) = node_semver::Version::parse(version) else {
        return false;
    };
    vulnerable_versions
        .iter()
        .filter_map(|range| node_semver::Range::parse(range).ok())
        .any(|range| version.satisfies(&range))
}

/// Atomic package.json write via tempfile + rename. Crash or Ctrl+C
fn best_non_vulnerable(packument: &Packument, vulnerable_versions: &[String]) -> Option<String> {
    let vulnerable: Vec<node_semver::Range> = vulnerable_versions
        .iter()
        .filter_map(|range| node_semver::Range::parse(range).ok())
        .collect();
    let mut best: Option<(&str, node_semver::Version)> = None;
    for ver_str in packument.versions.keys() {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !version.pre_release.is_empty() {
            continue;
        }
        if vulnerable.iter().any(|range| version.satisfies(range)) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, current)| version > *current) {
            best = Some((ver_str.as_str(), version));
        }
    }
    best.map(|(raw, _)| raw.to_string())
}

/// Reachable packages from the filtered roots, keyed by `dep_path`.

#[derive(Debug, Clone)]
struct Row {
    name: String,
    severity: Severity,
    title: String,
    vulnerable_versions: String,
    url: String,
}

fn flatten_advisories(v: &serde_json::Value, threshold: Severity) -> Vec<Row> {
    let Some(obj) = v.as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<Row> = Vec::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        for adv in arr {
            let sev_str = adv
                .get("severity")
                .and_then(|s| s.as_str())
                .unwrap_or("low");
            let Ok(sev) = sev_str.parse::<Severity>() else {
                continue;
            };
            if sev < threshold {
                continue;
            }
            rows.push(Row {
                name: name.clone(),
                severity: sev,
                title: adv
                    .get("title")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                vulnerable_versions: adv
                    .get("vulnerable_versions")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: adv
                    .get("url")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    rows.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.title.cmp(&b.title))
    });
    rows
}

/// Direct-dependency counts by category, consistent with the
/// `filter` / `no_optional` gating used to build the audited closure.
/// Mirrors pnpm's `metadata.dependencies`/`devDependencies`/
/// `optionalDependencies`/`totalDependencies`.
struct DependencyCounts {
    dependencies: usize,
    dev_dependencies: usize,
    optional_dependencies: usize,
    total: usize,
}

/// Count root deps by `dep_type`, honoring the SAME gating
/// `collect_dep_closure` applies (`filter.keeps` + the `no_optional`
/// drop), so the metadata reflects exactly what was audited. `total`
/// is the size of the transitive closure.
fn count_dependencies(
    graph: &aube_lockfile::LockfileGraph,
    filter: DepFilter,
    no_optional: bool,
    closure: &BTreeMap<String, &aube_lockfile::LockedPackage>,
) -> DependencyCounts {
    use aube_lockfile::DepType;
    let mut counts = DependencyCounts {
        dependencies: 0,
        dev_dependencies: 0,
        optional_dependencies: 0,
        total: closure.len(),
    };
    for dep in graph.root_deps() {
        if !filter.keeps(dep.dep_type) {
            continue;
        }
        if no_optional && matches!(dep.dep_type, DepType::Optional) {
            continue;
        }
        match dep.dep_type {
            DepType::Production => counts.dependencies += 1,
            DepType::Dev => counts.dev_dependencies += 1,
            DepType::Optional => counts.optional_dependencies += 1,
        }
    }
    counts
}

/// Build pnpm's `audit --json` report (`{ advisories, metadata }`) from
/// the post-ignore bulk response `raw` (`{ "<pkg>": [advisory, …] }`).
///
/// * `metadata.vulnerabilities` counts ONE per unique advisory `id`,
///   bucketed by severity into the five always-present, zero-filled
///   keys. Unknown severities are skipped (matching pnpm's
///   `isKnownSeverity`). Counted over ALL post-ignore advisories, BEFORE
///   the audit-level filter.
/// * `advisories` is keyed by `String(id)` (dedup: same id across
///   packages collapses to one entry, last write wins) and is
///   additionally filtered to advisories at or above `threshold`.
fn build_audit_report(
    raw: &serde_json::Value,
    threshold: Severity,
    deps: DependencyCounts,
) -> serde_json::Value {
    use serde_json::{Map, Value};

    let mut vulns: BTreeMap<&str, u64> = BTreeMap::from([
        ("info", 0),
        ("low", 0),
        ("moderate", 0),
        ("high", 0),
        ("critical", 0),
    ]);
    // De-dup the metadata count by advisory id so an advisory hitting
    // several packages is counted once, matching the `advisories` map.
    let mut counted_ids: BTreeSet<String> = BTreeSet::new();
    let mut advisories: Map<String, Value> = Map::new();

    if let Some(obj) = raw.as_object() {
        for advisory_list in obj.values() {
            let Some(arr) = advisory_list.as_array() else {
                continue;
            };
            for adv in arr {
                let Some(id) = advisory_id(adv) else {
                    continue;
                };
                let severity = adv.get("severity").and_then(|s| s.as_str());

                // metadata.vulnerabilities: one per unique id, known
                // severities only, BEFORE the level filter.
                if counted_ids.insert(id.clone())
                    && let Some(sev) = severity
                    && let Some(bucket) = vulns.get_mut(sev)
                {
                    *bucket += 1;
                }

                // advisories map: re-keyed by id, level-filtered.
                if severity
                    .and_then(|s| s.parse::<Severity>().ok())
                    .is_some_and(|s| s >= threshold)
                {
                    advisories.insert(id, adv.clone());
                }
            }
        }
    }

    serde_json::json!({
        "advisories": Value::Object(advisories),
        "metadata": {
            "vulnerabilities": {
                "info": vulns["info"],
                "low": vulns["low"],
                "moderate": vulns["moderate"],
                "high": vulns["high"],
                "critical": vulns["critical"],
            },
            "dependencies": deps.dependencies,
            "devDependencies": deps.dev_dependencies,
            "optionalDependencies": deps.optional_dependencies,
            "totalDependencies": deps.total,
        },
    })
}

/// Extract an advisory's npm `id` as a string. Numbers stringify; an
/// already-string id passes through. Missing/other → `None` (the
/// advisory is skipped, since pnpm keys solely on `id`).
fn advisory_id(adv: &serde_json::Value) -> Option<String> {
    match adv.get("id") {
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn render_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("No known vulnerabilities found");
        return;
    }

    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(7).max(7);
    let sev_w = 8; // "critical"
    let vul_w = rows
        .iter()
        .map(|r| r.vulnerable_versions.len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!(
        "{:<sev_w$}  {:<name_w$}  {:<vul_w$}  Title",
        "Severity", "Package", "Vulnerable",
    );
    for row in rows {
        println!(
            "{:<sev_w$}  {:<name_w$}  {:<vul_w$}  {}",
            row.severity, row.name, row.vulnerable_versions, row.title,
        );
        if !row.url.is_empty() {
            println!("{:<sev_w$}  {:<name_w$}  {}", "", "", row.url);
        }
    }
    println!();
    println!(
        "{} found",
        pluralizer::pluralize("vulnerability", rows.len() as isize, true)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packument_with_versions(versions: &[&str]) -> Packument {
        let mut packument = Packument {
            name: "demo".to_string(),
            modified: None,
            versions: BTreeMap::new(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        for version in versions {
            packument.versions.insert(
                (*version).to_string(),
                aube_registry::VersionMetadata {
                    name: "demo".to_string(),
                    version: (*version).to_string(),
                    dependencies: BTreeMap::new(),
                    dev_dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    peer_dependencies_meta: BTreeMap::new(),
                    optional_dependencies: BTreeMap::new(),
                    bundled_dependencies: None,
                    dist: None,
                    os: Vec::new(),
                    cpu: Vec::new(),
                    libc: Vec::new(),
                    engines: BTreeMap::new(),
                    license: None,
                    funding_url: None,
                    bin: BTreeMap::new(),
                    has_install_script: false,
                    deprecated: None,
                    approver: None,
                    npm_user: None,
                },
            );
        }
        packument
    }

    #[test]
    fn audit_fix_does_not_select_prerelease_versions() {
        let packument = packument_with_versions(&["1.0.0", "1.5.0", "2.0.0-beta.1"]);

        assert_eq!(
            best_non_vulnerable(&packument, &["<1.5.0".to_string()]),
            Some("1.5.0".to_string())
        );
    }

    #[test]
    fn audit_picker_preselects_every_advisory() {
        let rows = vec![
            Row {
                name: "is-number".to_string(),
                severity: Severity::High,
                title: "number advisory".to_string(),
                vulnerable_versions: "<7.0.0".to_string(),
                url: "https://example.test/number".to_string(),
            },
            Row {
                name: "is-odd".to_string(),
                severity: Severity::Moderate,
                title: "odd advisory".to_string(),
                vulnerable_versions: "<3.0.0".to_string(),
                url: String::new(),
            },
        ];

        let picker = advisory_picker(&rows);

        assert_eq!(picker.min, 1);
        assert!(picker.filterable);
        assert_eq!(picker.options.len(), 2);
        assert!(picker.options.iter().all(|option| option.selected));
        assert_eq!(picker.options[0].item, 0);
        assert_eq!(
            picker.options[0].label,
            "high is-number <7.0.0 number advisory"
        );
        assert_eq!(
            picker.options[0].description.as_deref(),
            Some("https://example.test/number")
        );
        assert_eq!(picker.options[1].item, 1);
    }

    #[test]
    fn audit_update_keeps_manifest_range_when_new_version_satisfies_it() {
        assert!(spec_satisfies_version(">=0.1.0", "7.0.0"));
        assert!(spec_satisfies_version("npm:is-number@>=0.1.0", "7.0.0"));
        assert!(!spec_satisfies_version("3.0.0", "7.0.0"));
    }

    #[test]
    fn audit_update_rewrites_compound_ranges_to_safe_floor() {
        assert_eq!(
            rewrite_specifier("^1.0.0 || ^2.0.0", "is-number", "7.0.0", None),
            ">=7.0.0"
        );
        assert_eq!(
            rewrite_specifier(">=1.0.0 <7.0.0", "is-number", "7.0.0", None),
            ">=7.0.0"
        );
        assert_eq!(
            rewrite_specifier("npm:is-number@^1.0.0 || ^2.0.0", "is-number", "7.0.0", None),
            "npm:is-number@>=7.0.0"
        );
    }

    #[test]
    fn audit_update_tracks_partially_fixed_package_updates() {
        let rows = vec![Row {
            name: "is-number".to_string(),
            severity: Severity::High,
            title: "number advisory".to_string(),
            vulnerable_versions: "<7.0.0".to_string(),
            url: String::new(),
        }];
        let before = BTreeMap::from([(
            "is-number".to_string(),
            BTreeSet::from(["3.0.0".to_string()]),
        )]);
        let after = BTreeMap::from([(
            "is-number".to_string(),
            BTreeSet::from(["7.0.0".to_string()]),
        )]);

        assert_eq!(
            updated_package_names(&rows, &before, &after),
            BTreeSet::from(["is-number".to_string()])
        );
    }

    #[test]
    fn filter_ignored_drops_matching_ghsa_and_cve_and_numeric_id() {
        let raw = serde_json::json!({
            "pkg-a": [
                {
                    "id": 1404,
                    "severity": "high",
                    "github_advisory_id": "GHSA-xxxx-aaaa-bbbb",
                    "cves": ["CVE-2022-1111"],
                    "title": "a",
                    "vulnerable_versions": "<1.0.0",
                    "url": "https://example.test/a"
                },
                {
                    "id": 1405,
                    "severity": "low",
                    "github_advisory_id": "GHSA-yyyy-cccc-dddd",
                    "cves": [],
                    "title": "b",
                    "vulnerable_versions": "<2.0.0",
                    "url": "https://example.test/b"
                }
            ],
            "pkg-b": [{
                "id": 1406,
                "severity": "critical",
                "github_advisory_id": "GHSA-zzzz-eeee-ffff",
                "cves": [],
                "title": "c",
                "vulnerable_versions": "<3.0.0",
                "url": "https://example.test/c"
            }]
        });

        // Match by GHSA (case-insensitive) drops the high advisory on pkg-a
        // but leaves its low advisory and all of pkg-b intact.
        let out = filter_ignored_ids(&raw, &["ghsa-xxxx-aaaa-bbbb".to_string()]);
        let pkg_a = out.get("pkg-a").and_then(|v| v.as_array()).unwrap();
        assert_eq!(pkg_a.len(), 1);
        assert_eq!(pkg_a[0].get("title").unwrap(), "b");
        assert!(out.get("pkg-b").is_some());

        // Match by CVE also drops just the high advisory.
        let out = filter_ignored_ids(&raw, &["CVE-2022-1111".to_string()]);
        let pkg_a = out.get("pkg-a").and_then(|v| v.as_array()).unwrap();
        assert_eq!(pkg_a.len(), 1);
        assert_eq!(pkg_a[0].get("title").unwrap(), "b");

        // Match by numeric id as string works too.
        let out = filter_ignored_ids(&raw, &["1406".to_string()]);
        assert!(out.get("pkg-b").is_none());
        assert!(out.get("pkg-a").is_some());

        // Dropping every advisory on a package removes the package entry.
        let out = filter_ignored_ids(
            &raw,
            &["1404".to_string(), "GHSA-yyyy-cccc-dddd".to_string()],
        );
        assert!(out.get("pkg-a").is_none());
        assert!(out.get("pkg-b").is_some());
    }

    #[test]
    fn configured_audit_ignores_reads_ghsa_and_legacy_cve_keys() {
        let manifest = aube_manifest::PackageJson::parse(
            std::path::Path::new("package.json"),
            r#"{
              "name": "demo",
              "version": "1.0.0",
              "auditConfig": {
                "ignoreGhsas": ["GHSA-aaaa-bbbb-cccc"],
                "ignoreCves": ["CVE-2099-0001"]
              }
            }"#
            .to_string(),
        )
        .unwrap();
        let ignores = configured_audit_ignores(&manifest, &["1404".to_string()]);
        assert_eq!(
            ignores,
            vec![
                "1404".to_string(),
                "GHSA-aaaa-bbbb-cccc".to_string(),
                "CVE-2099-0001".to_string()
            ]
        );
    }

    #[test]
    fn unfixable_filter_threshold_uses_all_severities_for_json_metadata() {
        assert_eq!(
            unfixable_filter_threshold(true, Severity::High),
            Severity::Info
        );
        assert_eq!(
            unfixable_filter_threshold(false, Severity::High),
            Severity::High
        );
    }

    #[test]
    fn build_audit_report_keys_by_id_dedups_and_level_filters() {
        // `dep-a` and `dep-b` share advisory 100 (a high CVE): the
        // `advisories` map must collapse it to one entry keyed by the
        // id string, and the metadata must count it once. Advisory 200
        // is low; with a `high` threshold it falls out of the
        // `advisories` map but still counts in the metadata.
        let raw = serde_json::json!({
            "dep-a": [
                { "id": 100, "severity": "high", "title": "shared high" },
                { "id": 200, "severity": "low", "title": "a-only low" }
            ],
            "dep-b": [
                { "id": 100, "severity": "high", "title": "shared high" }
            ]
        });
        let deps = DependencyCounts {
            dependencies: 2,
            dev_dependencies: 0,
            optional_dependencies: 0,
            total: 2,
        };

        let report = build_audit_report(&raw, Severity::High, deps);

        // (a) advisories keyed by id STRING, not package name.
        let advisories = report["advisories"].as_object().unwrap();
        assert!(advisories.contains_key("100"));
        assert!(!advisories.contains_key("dep-a"));
        // (c) same id under two packages collapses to one entry.
        // (d) the below-threshold low advisory is dropped from the map.
        assert_eq!(advisories.len(), 1);
        assert!(!advisories.contains_key("200"));

        // (b) metadata.vulnerabilities carries all five severity keys.
        let vulns = report["metadata"]["vulnerabilities"].as_object().unwrap();
        for key in ["info", "low", "moderate", "high", "critical"] {
            assert!(vulns.contains_key(key), "missing severity bucket {key}");
        }
        // (c) dedup: advisory 100 counted once despite two occurrences.
        assert_eq!(vulns["high"], 1);
        // (d) the low advisory still counts in metadata even though the
        // level filter dropped it from the advisories map.
        assert_eq!(vulns["low"], 1);
        assert_eq!(vulns["info"], 0);

        assert_eq!(report["metadata"]["totalDependencies"], 2);
        assert_eq!(report["metadata"]["dependencies"], 2);
    }

    #[test]
    fn build_audit_report_handles_info_severity_consistently() {
        // npm's bulk endpoint can return `severity: "info"` advisories.
        // They must (a) count in `metadata.vulnerabilities.info`, (b) be
        // excluded from the `advisories` map at the default `low`
        // threshold, and (c) ENTER the map under `--audit-level info` —
        // i.e. behave like every other severity. (Before `Severity::Info`
        // existed, info advisories counted in metadata but could never
        // appear in `advisories`, regardless of threshold.)
        let raw = serde_json::json!({
            "dep-a": [{ "id": 300, "severity": "info", "title": "info note" }]
        });
        let deps = || DependencyCounts {
            dependencies: 1,
            dev_dependencies: 0,
            optional_dependencies: 0,
            total: 1,
        };

        let at_low = build_audit_report(&raw, Severity::Low, deps());
        assert_eq!(at_low["metadata"]["vulnerabilities"]["info"], 1);
        assert!(
            at_low["advisories"].as_object().unwrap().is_empty(),
            "info advisory must be filtered out at the default `low` threshold"
        );

        let at_info = build_audit_report(&raw, Severity::Info, deps());
        assert_eq!(at_info["metadata"]["vulnerabilities"]["info"], 1);
        assert!(
            at_info["advisories"]
                .as_object()
                .unwrap()
                .contains_key("300"),
            "info advisory must appear in the map under `--audit-level info`"
        );
    }

    #[test]
    fn build_audit_report_empty_input_is_well_formed() {
        // An empty audit (no advisories) still emits the full pnpm shape:
        // a present-but-empty `advisories` map and zero-filled severity
        // buckets — never a bare `{}`. This is the user-visible format
        // contract consumers parse.
        let deps = DependencyCounts {
            dependencies: 3,
            dev_dependencies: 1,
            optional_dependencies: 0,
            total: 12,
        };
        let report = build_audit_report(&serde_json::json!({}), Severity::Low, deps);

        assert!(report["advisories"].as_object().unwrap().is_empty());
        let vulns = report["metadata"]["vulnerabilities"].as_object().unwrap();
        for key in ["info", "low", "moderate", "high", "critical"] {
            assert_eq!(vulns[key], 0, "severity bucket {key} must be zero-filled");
        }
        assert_eq!(report["metadata"]["dependencies"], 3);
        assert_eq!(report["metadata"]["devDependencies"], 1);
        assert_eq!(report["metadata"]["totalDependencies"], 12);
    }

    #[test]
    fn filter_ignored_with_empty_list_is_a_noop() {
        let raw = serde_json::json!({
            "pkg-a": [{
                "id": 1,
                "severity": "high",
                "github_advisory_id": "GHSA-a",
                "cves": [],
                "title": "t",
                "vulnerable_versions": "<1.0.0",
                "url": ""
            }]
        });
        let out = filter_ignored_ids(&raw, &[]);
        assert_eq!(out, raw);
    }
}
