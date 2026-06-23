use super::manifest::collect_workspace_versions;
use super::spec::parse_pkg_spec;
use std::path::Path;

pub(super) async fn run_cli_name_gates(
    cwd: &Path,
    packages: &[String],
    allow_low_downloads: bool,
) -> miette::Result<()> {
    let registry_names = registry_bound_names_for_supply_chain(cwd, packages);
    let (advisory_check, low_download_threshold, allowed_unpopular) =
        crate::commands::with_settings_ctx(cwd, |ctx| {
            let policy = if aube_settings::resolved::paranoid(ctx) {
                aube_settings::resolved::AdvisoryCheck::Required
            } else {
                aube_settings::resolved::advisory_check(ctx)
            };
            (
                policy,
                aube_settings::resolved::low_download_threshold(ctx),
                aube_settings::resolved::allowed_unpopular_packages(ctx).unwrap_or_default(),
            )
        });
    crate::commands::add_supply_chain::run_gates(
        &registry_names,
        advisory_check,
        low_download_threshold,
        allow_low_downloads,
        &allowed_unpopular,
    )
    .await
}

fn registry_bound_names_for_supply_chain(cwd: &Path, packages: &[String]) -> Vec<String> {
    let mut names = Vec::with_capacity(packages.len());
    let workspace_versions = collect_workspace_versions(cwd);
    // Scope→registry overrides + the default registry tell us which
    // names route through public npmjs. Anything else (a swapped-out
    // default registry, an `@myorg:registry=https://internal/`
    // override) has no signal in the OSV `MAL-*` database or the
    // npmjs weekly-downloads API — skip those names so private
    // packages don't trip the gates on a public-registry collision.
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    for raw in packages {
        let Ok(spec) = parse_pkg_spec(raw) else {
            // Parse failures get a richer diagnostic from
            // `update_manifest_for_add` later — we don't want to
            // double-report or block the gate on something that
            // would already fail.
            continue;
        };
        if spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.jsr_name.is_some()
            || aube_util::pkg::is_workspace_spec(&spec.range)
            || aube_util::pkg::is_catalog_spec(&spec.range)
        {
            continue;
        }
        // A bare `aube add my-pkg` against a local workspace sibling
        // resolves locally — no public registry round-trip happens,
        // so the OSV / downloads probes have nothing to say.
        if workspace_versions.contains_key(&spec.name) {
            continue;
        }
        if !npm_config.is_public_npmjs(&spec.name) {
            // `redact_url` strips any embedded userinfo (`https://tok@host/`
            // — uncommon but a registry URL can legally carry it) so a
            // token doesn't slip into observability pipelines that ingest
            // debug-level structured logs.
            tracing::debug!(
                "skipping supply-chain gates for {}: routes through non-public registry {}",
                spec.name,
                aube_util::url::redact_url(npm_config.registry_for(&spec.name))
            );
            continue;
        }
        // Scoped names (`@scope/name`) stay in the list. OSV's batch
        // API supports scoped queries — skipping them here would let
        // a `MAL-*` advisory against `@scope/evil` slip past the
        // hard block. The downloads probe already folds scoped
        // packages into `DownloadCount::Unknown` (npm's downloads
        // API doesn't index them), so the prompt naturally skips
        // them — no per-name special case needed in the gate.
        names.push(spec.name);
    }
    names.sort();
    names.dedup();
    names
}
