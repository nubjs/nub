//! Inspect npm publishing evidence for one exact package version.

use crate::commands::{make_client, packument_full_cache_dir, split_name_spec};
use miette::{IntoDiagnostic, WrapErr, miette};
use serde::Serialize;

pub(super) const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube trust check @hono/node-server@1.19.17
  @hono/node-server@1.19.17: pass
  published: 2026-07-27T03:07:48.993Z
  current evidence: trusted publisher
  strongest earlier evidence: trusted publisher (2.0.10)

  # Evaluate the underlying policy even when aube has a built-in exception
  $ aube trust check @hono/node-server@1.19.15 --ignore-default-excludes

  # Machine-readable report
  $ aube trust check @hono/node-server@1.19.17 --json
";

#[derive(Debug, usage_rs::Args)]
pub(super) struct CheckArgs {
    /// Exact npm package version to inspect
    ///
    /// Example: `@hono/node-server@1.19.17`.
    #[usage(arg, name = "PACKAGE@VERSION")]
    pub package: String,
    /// Evaluate the policy without aube's built-in package exceptions
    #[usage(long)]
    pub ignore_default_excludes: bool,
    /// Emit a JSON report
    #[usage(long)]
    pub json: bool,
    #[usage(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CheckStatus {
    Downgrade,
    Excluded,
    MissingTime,
    Pass,
    Unverifiable,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EarlierEvidence {
    evidence: &'static str,
    published_at: Option<String>,
    version: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrustReport {
    package: String,
    version: String,
    published_at: Option<String>,
    current_evidence: Option<&'static str>,
    strongest_earlier_evidence: Option<EarlierEvidence>,
    default_excluded: bool,
    status: CheckStatus,
    underlying_status: CheckStatus,
}

pub(super) async fn run(args: CheckArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let (name, version) = split_name_spec(&args.package);
    let Some(version) = version else {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_NO_MATCHING_VERSION,
            "exact package version required, for example `{} trust check lodash@4.17.21`",
            aube_util::prog()
        ));
    };

    let cwd = crate::dirs::project_root_or_cwd()?;
    let client = make_client(&cwd);
    let packument = client
        .fetch_packument_with_time_cached(name, &packument_full_cache_dir())
        .await
        .map_err(|e| match e {
            aube_registry::Error::NotFound(package) => miette!(
                code = aube_codes::errors::ERR_AUBE_REGISTRY_ERROR,
                "package not found: {package}"
            ),
            other => miette!(
                code = aube_codes::errors::ERR_AUBE_REGISTRY_ERROR,
                "failed to fetch {name}: {other}"
            ),
        })?;
    let version_meta = packument.versions.get(version).ok_or_else(|| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_NO_MATCHING_VERSION,
            "version {version} not present in packument for {name}"
        )
    })?;

    let report = build_report(&packument, version, args.ignore_default_excludes);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .into_diagnostic()
                .wrap_err("failed to serialize trust report")?
        );
    } else {
        print_report(&report);
    }

    let defaults = aube_resolver::TrustExcludeRules::default();
    let empty = aube_resolver::TrustExcludeRules::empty();
    let effective_excludes = if args.ignore_default_excludes {
        &empty
    } else {
        &defaults
    };
    aube_resolver::check_no_downgrade(&packument, version, version_meta, effective_excludes, None)
        .map_err(|err| match err {
            aube_resolver::TrustCheckError::Downgrade(details) => {
                aube_resolver::Error::TrustDowngrade(Box::new(details))
            }
            aube_resolver::TrustCheckError::MissingTime(details) => {
                aube_resolver::Error::TrustCheckMissingTime(Box::new(details))
            }
        })
        .map_err(miette::Report::new)
}

fn build_report(
    packument: &aube_registry::Packument,
    version: &str,
    ignore_default_excludes: bool,
) -> TrustReport {
    let version_meta = &packument.versions[version];
    let defaults = aube_resolver::TrustExcludeRules::default();
    let default_excluded = defaults.excludes(&packument.name, version);
    let underlying = aube_resolver::check_no_downgrade(
        packument,
        version,
        version_meta,
        &aube_resolver::TrustExcludeRules::empty(),
        None,
    );
    let underlying_status = status_for(packument, &underlying);
    let status = if default_excluded && !ignore_default_excludes {
        CheckStatus::Excluded
    } else {
        underlying_status
    };
    let prior =
        aube_resolver::strongest_prior_evidence(packument, version).map(|prior| EarlierEvidence {
            evidence: evidence_name(prior.evidence),
            published_at: packument.time.get(&prior.version).cloned(),
            version: prior.version,
        });
    TrustReport {
        package: packument.name.clone(),
        version: version.to_string(),
        published_at: packument.time.get(version).cloned(),
        current_evidence: aube_resolver::evidence_for(version_meta).map(evidence_name),
        strongest_earlier_evidence: prior,
        default_excluded,
        status,
        underlying_status,
    }
}

fn status_for(
    packument: &aube_registry::Packument,
    result: &Result<(), aube_resolver::TrustCheckError>,
) -> CheckStatus {
    match result {
        Err(aube_resolver::TrustCheckError::Downgrade(_)) => CheckStatus::Downgrade,
        Err(aube_resolver::TrustCheckError::MissingTime(_)) => CheckStatus::MissingTime,
        Ok(()) if packument.time.is_empty() => CheckStatus::Unverifiable,
        Ok(()) => CheckStatus::Pass,
    }
}

fn evidence_name(evidence: aube_resolver::TrustEvidence) -> &'static str {
    match evidence {
        aube_resolver::TrustEvidence::StagedPublish => "staged-publish",
        aube_resolver::TrustEvidence::TrustedPublisher => "trusted-publisher",
        aube_resolver::TrustEvidence::Provenance => "provenance",
    }
}

fn print_report(report: &TrustReport) {
    println!(
        "{}@{}: {}",
        report.package,
        report.version,
        status_name(report.status)
    );
    println!(
        "published: {}",
        report.published_at.as_deref().unwrap_or("missing")
    );
    println!(
        "current evidence: {}",
        report
            .current_evidence
            .map(evidence_display)
            .unwrap_or("none")
    );
    match &report.strongest_earlier_evidence {
        Some(prior) => println!(
            "strongest earlier evidence: {} ({})",
            evidence_display(prior.evidence),
            prior.version
        ),
        None => println!("strongest earlier evidence: none"),
    }
    if report.default_excluded {
        println!("built-in exception: yes");
        if report.status == CheckStatus::Excluded {
            println!(
                "underlying policy: {} (use --ignore-default-excludes to enforce)",
                status_name(report.underlying_status)
            );
        }
    }
}

fn evidence_display(evidence: &'static str) -> &'static str {
    match evidence {
        "staged-publish" => "staged publish approval",
        "trusted-publisher" => "trusted publisher",
        "provenance" => "provenance attestation",
        other => other,
    }
}

fn status_name(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Downgrade => "downgrade",
        CheckStatus::Excluded => "excluded",
        CheckStatus::MissingTime => "missing-time",
        CheckStatus::Pass => "pass",
        CheckStatus::Unverifiable => "unverifiable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hono_packument() -> aube_registry::Packument {
        serde_json::from_value(serde_json::json!({
            "name": "@hono/node-server",
            "versions": {
                "2.0.10": {
                    "name": "@hono/node-server",
                    "version": "2.0.10",
                    "_npmUser": {"trustedPublisher": {"id": "github"}}
                },
                "1.19.15": {
                    "name": "@hono/node-server",
                    "version": "1.19.15"
                },
                "1.19.17": {
                    "name": "@hono/node-server",
                    "version": "1.19.17",
                    "_npmUser": {"trustedPublisher": {"id": "github"}}
                }
            },
            "time": {
                "2.0.10": "2026-07-15T23:20:08.214Z",
                "1.19.15": "2026-07-24T00:46:09.652Z",
                "1.19.17": "2026-07-27T03:07:48.993Z"
            }
        }))
        .unwrap()
    }

    #[test]
    fn report_distinguishes_default_exception_from_underlying_downgrade() {
        let packument = hono_packument();
        let report = build_report(&packument, "1.19.15", false);
        assert_eq!(report.status, CheckStatus::Excluded);
        assert_eq!(report.underlying_status, CheckStatus::Downgrade);
        assert!(report.default_excluded);

        let enforced = build_report(&packument, "1.19.15", true);
        assert_eq!(enforced.status, CheckStatus::Downgrade);
    }

    #[test]
    fn report_passes_restored_trusted_publish() {
        let report = build_report(&hono_packument(), "1.19.17", false);
        assert_eq!(report.status, CheckStatus::Pass);
        assert_eq!(report.current_evidence, Some("trusted-publisher"));
        assert!(!report.default_excluded);
        assert_eq!(
            report.strongest_earlier_evidence.map(|prior| prior.version),
            Some("2.0.10".to_string())
        );
    }
}
