use super::manifest::collect_workspace_versions;
use super::spec::parse_pkg_spec;
use std::path::Path;

pub(super) async fn run_cli_name_gates(
    cwd: &Path,
    packages: &[String],
    allow_low_downloads: bool,
) -> miette::Result<()> {
    let registry_inputs = registry_bound_inputs_for_supply_chain(cwd, packages);
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
        &registry_inputs.exact_advisory_pairs,
        &registry_inputs.download_names,
        advisory_check,
        low_download_threshold,
        allow_low_downloads,
        &allowed_unpopular,
    )
    .await
}

#[derive(Default)]
struct RegistryBoundSupplyChainInputs {
    exact_advisory_pairs: Vec<(String, String)>,
    download_names: Vec<String>,
}

fn registry_bound_inputs_for_supply_chain(
    cwd: &Path,
    packages: &[String],
) -> RegistryBoundSupplyChainInputs {
    let mut inputs = RegistryBoundSupplyChainInputs {
        exact_advisory_pairs: Vec::with_capacity(packages.len()),
        download_names: Vec::with_capacity(packages.len()),
    };
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
        inputs.download_names.push(spec.name.clone());
        // Only a FULL exact pin (`name@x.y.z`) gets a version-aware
        // pre-resolve OSV block here. Bare / dist-tag / partial-range
        // specs are intentionally NOT name-blocked: a name-only OSV
        // query flags a package because SOME version is malicious, which
        // wrongly rejects a popular package with a *patched* malicious
        // version (axios's MAL-2026-2307 covers only 0.30.4 + 1.14.1,
        // yet `aube add axios` would resolve to a clean 1.18.1). The
        // resolver hasn't run yet, so we can't version-check these here
        // — the post-resolve transitive OSV gate (`osv_transitive_check`
        // → `run_transitive_osv_gate`, version-aware over the full
        // resolved graph) is the authoritative block. An all-versions
        // typosquat resolves to a malicious version and is still caught
        // there; only the patched-popular-package case newly succeeds.
        if spec.has_explicit_range && is_full_exact_version(&spec.range) {
            inputs.exact_advisory_pairs.push((spec.name, spec.range));
        }
    }
    inputs.exact_advisory_pairs.sort();
    inputs.exact_advisory_pairs.dedup();
    inputs.download_names.sort();
    inputs.download_names.dedup();
    inputs
}

fn is_full_exact_version(range: &str) -> bool {
    let suffix_start = range.find(['-', '+']).unwrap_or(range.len());
    let core = &range[..suffix_start];
    let mut parts = core.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    [major, minor, patch]
        .into_iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && node_semver::Version::parse(range).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_bound_inputs_use_versioned_osv_for_exact_versions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inputs = registry_bound_inputs_for_supply_chain(tmp.path(), &["nx@23.0.0".into()]);

        assert_eq!(
            inputs.exact_advisory_pairs,
            vec![("nx".to_string(), "23.0.0".to_string())],
        );
        assert_eq!(inputs.download_names, vec!["nx".to_string()]);
    }

    #[test]
    fn registry_bound_inputs_skip_pre_resolve_osv_for_ranges_and_tags() {
        // Bare / dist-tag / partial-range specs get NO pre-resolve OSV
        // pair — a name-only block would reject a popular package whose
        // malicious version has since been patched (issue #88: axios).
        // The post-resolve transitive gate is the authoritative check
        // for these. The downloads gate still sees every name.
        let tmp = tempfile::tempdir().expect("tempdir");
        let inputs = registry_bound_inputs_for_supply_chain(
            tmp.path(),
            &[
                "nx@^23".into(),
                "pkg-major@4".into(),
                "pkg-minor@1.2".into(),
                "react".into(),
                "vite@latest".into(),
            ],
        );

        assert!(
            inputs.exact_advisory_pairs.is_empty(),
            "ranges/tags must not be pre-resolve OSV-checked"
        );
        assert_eq!(
            inputs.download_names,
            vec![
                "nx".to_string(),
                "pkg-major".to_string(),
                "pkg-minor".to_string(),
                "react".to_string(),
                "vite".to_string(),
            ],
        );
    }

    #[test]
    fn bare_spec_with_patched_malicious_version_is_not_pre_resolve_blocked() {
        // Issue #88 regression: `aube add axios` must NOT be name-only
        // OSV-blocked. axios has MAL-2026-2307 (affecting only 0.30.4 +
        // 1.14.1), so the old name-only pre-resolve gate hard-rejected
        // every bare/dist-tag install even though it resolves to a clean
        // 1.18.1. A bare spec now produces no pre-resolve advisory pair;
        // the post-resolve versioned gate is the authoritative block,
        // and it clears a clean resolved version while still catching a
        // malicious one. An explicit malicious EXACT pin
        // (`axios@1.14.1`) still routes to the version-aware pre-check.
        let tmp = tempfile::tempdir().expect("tempdir");

        let bare = registry_bound_inputs_for_supply_chain(tmp.path(), &["axios".into()]);
        assert!(
            bare.exact_advisory_pairs.is_empty(),
            "bare `axios` must not be pre-resolve OSV-blocked (issue #88)"
        );
        assert_eq!(bare.download_names, vec!["axios".to_string()]);

        let pinned = registry_bound_inputs_for_supply_chain(tmp.path(), &["axios@1.14.1".into()]);
        assert_eq!(
            pinned.exact_advisory_pairs,
            vec![("axios".to_string(), "1.14.1".to_string())],
            "an exact malicious pin still gets the version-aware pre-check"
        );
    }

    #[test]
    fn full_exact_version_requires_major_minor_patch() {
        assert!(is_full_exact_version("1.2.3"));
        assert!(is_full_exact_version("1.2.3-beta.1"));
        assert!(is_full_exact_version("1.2.3+build.7"));
        assert!(!is_full_exact_version("1"));
        assert!(!is_full_exact_version("1.2"));
        assert!(!is_full_exact_version("^1.2.3"));
        assert!(!is_full_exact_version("latest"));
    }

    #[test]
    fn registry_bound_inputs_version_alias_checks_real_package() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inputs =
            registry_bound_inputs_for_supply_chain(tmp.path(), &["nx-stable@npm:nx@23.0.0".into()]);

        assert_eq!(
            inputs.exact_advisory_pairs,
            vec![("nx".to_string(), "23.0.0".to_string())],
        );
        assert_eq!(inputs.download_names, vec!["nx".to_string()]);
    }
}
