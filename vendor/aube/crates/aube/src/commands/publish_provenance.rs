//! SLSA provenance attestation for `aube publish --provenance`.
//!
//! Produces a Sigstore v0.3 bundle over an in-toto SLSA v1 statement whose
//! subject is the tarball we're about to PUT. The flow mirrors what npm's
//! libnpmpublish does when `--provenance` is set:
//!
//!   1. Detect an ambient OIDC token via `ambient-id` (GitHub Actions,
//!      GitLab CI, Buildkite, CircleCI). Audience is `sigstore`, matching
//!      what Sigstore's public-good Fulcio instance expects.
//!   2. Build the SLSA v1 `buildDefinition` / `runDetails` predicate from
//!      the CI runner's environment variables — right now we only fill
//!      the GitHub Actions shape, which is the only one npm itself honors.
//!   3. Sign via `sigstore-sign`: ephemeral keypair → Fulcio cert → DSSE
//!      envelope over the in-toto statement → Rekor tlog → optional TSA
//!      timestamp → serialized Sigstore bundle JSON.
//!
//! The caller (`commands::publish`) base64-encodes the bundle and stuffs
//! it into the npm publish body's `_attachments` under
//! `<name>-<version>.sigstore` with content_type
//! `application/vnd.dev.sigstore.bundle+json;version=0.3` — that's the
//! shape libnpmpublish produces, and what registries reading the v1
//! attestations API expect.
//!
//! Outside a supported CI environment this module errors with a clear
//! "need OIDC" message rather than silently producing an unsigned bundle.

use miette::{IntoDiagnostic, miette};
use sha2::{Digest as _, Sha512};
use sigstore_oidc::IdentityToken;
use sigstore_sign::SigningContext;
use sigstore_types::{Digest, Statement, Subject};

/// Predicate type for SLSA v1 provenance. The Sigstore bundle the registry
/// surfaces in the npm UI only lights up as "provenance" when this exact URI
/// is in the in-toto statement.
const SLSA_V1_PREDICATE_TYPE: &str = "https://slsa.dev/provenance/v1";

/// Build type npm uses for GitHub Actions workflow provenance. Must match
/// the SLSA GitHub Actions buildtype spec — changing this silently breaks
/// verification for downstream consumers.
const GITHUB_ACTIONS_BUILD_TYPE: &str =
    "https://slsa-framework.github.io/github-actions-buildtypes/workflow/v1";

/// Probe the ambient environment for an OIDC token without running the
/// full Fulcio/Rekor signing flow. Used by `--dry-run --provenance` so
/// users verifying their CI setup find out *now* whether OIDC is wired
/// up, rather than on the real publish. `NPM_ID_TOKEN` is parsed locally
/// because the registry has already minted it; otherwise a successful
/// `ambient-id` detection is proof that the runner's token endpoint is
/// reachable and the workload identity is configured, which is what the
/// dry-run is actually testing for.
pub async fn probe_oidc_available() -> miette::Result<()> {
    detect_oidc_token().await.map(|_| ())
}

/// Generate a Sigstore bundle attesting to `tarball_bytes` being built from
/// the current CI run. Returns the serialized bundle JSON, ready to be
/// base64-encoded into the npm publish body.
///
/// The in-toto subject mirrors what `libnpmpublish` produces so npm's
/// server-side verification accepts it:
///   - `name`: a `pkg:npm/<name>@<version>` purl, with `@` in scoped names
///     percent-encoded (`@scope/foo` → `%40scope/foo`).
///   - `digest.sha512`: hex-encoded SHA-512 of the tarball — matching the
///     `dist.integrity` field the registry stores for the published
///     version, which is what npm's verifier compares against.
pub async fn generate(
    tarball_bytes: &[u8],
    package_name: &str,
    package_version: &str,
) -> miette::Result<String> {
    let token = detect_oidc_token().await?;
    let predicate = build_slsa_predicate()?;

    let sha512_hex = hex::encode(Sha512::digest(tarball_bytes));

    let statement = Statement {
        type_: "https://in-toto.io/Statement/v1".to_string(),
        subject: vec![Subject {
            name: npm_purl(package_name, package_version),
            digest: Digest {
                sha256: None,
                sha512: Some(sha512_hex),
            },
        }],
        predicate_type: SLSA_V1_PREDICATE_TYPE.to_string(),
        predicate,
    };
    let statement_json = serde_json::to_vec(&statement)
        .map_err(|e| miette!("failed to serialize in-toto statement: {e}"))?;

    let signer = SigningContext::production().signer(token);
    let bundle = signer
        .sign_raw_statement(&statement_json)
        .await
        .map_err(|e| miette!("sigstore signing failed: {e}"))?;

    bundle
        .to_json()
        .map_err(|e| miette!("failed to serialize sigstore bundle: {e}"))
}

/// Build an npm purl the way `libnpmpublish` does: only `@` is
/// percent-encoded (to `%40`), the `/` between scope and name is left
/// alone. This matches the canonical purl form for npm packages and is
/// what the npm registry's provenance verifier matches against.
fn npm_purl(name: &str, version: &str) -> String {
    let encoded_name = name.replace('@', "%40");
    format!("pkg:npm/{encoded_name}@{version}")
}

/// Ask `ambient-id` for an OIDC token with audience `sigstore`. We want a
/// hard error (not `None`) outside CI — `--provenance` was explicitly
/// requested, so silently falling back to an unsigned publish would defeat
/// the whole point of the flag.
async fn detect_oidc_token() -> miette::Result<IdentityToken> {
    if let Ok(token) = std::env::var("NPM_ID_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return IdentityToken::from_jwt(token)
                .into_diagnostic()
                .map_err(|e| e.wrap_err("failed to parse NPM_ID_TOKEN as JWT"));
        }
    }

    let detector = ambient_id::Detector::new();
    let token = detector
        .detect("sigstore")
        .await
        .map_err(|e| miette!("OIDC detection failed: {e}"))?
        .ok_or_else(|| {
            miette!(
                "--provenance requires an OIDC-capable CI environment \
                 (GitHub Actions with `id-token: write`, GitLab CI, \
                 Buildkite, or CircleCI). No ambient credentials detected."
            )
        })?;

    IdentityToken::from_jwt(token.reveal())
        .into_diagnostic()
        .map_err(|e| e.wrap_err("failed to parse detected OIDC token as JWT"))
}

/// Construct the SLSA v1 provenance predicate from the current environment.
/// Today we only populate the GitHub Actions shape — other CIs (GitLab,
/// Buildkite, CircleCI) get a stub that names the builder but leaves the
/// workflow-specific fields empty, which npm will accept as a bundle but
/// won't light up as "verified" in their UI. That's an explicit trade: we
/// want the flag to *work* on every OIDC-capable runner, even if full
/// provenance fidelity is GitHub-only for now.
fn build_slsa_predicate() -> miette::Result<serde_json::Value> {
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        return Ok(github_actions_predicate());
    }
    Ok(generic_predicate())
}

fn github_actions_predicate() -> serde_json::Value {
    let env = |k: &str| std::env::var(k).unwrap_or_default();

    let server_url = env("GITHUB_SERVER_URL");
    let repository = env("GITHUB_REPOSITORY");
    let repo_url = if server_url.is_empty() || repository.is_empty() {
        String::new()
    } else {
        format!("{server_url}/{repository}")
    };

    // GITHUB_WORKFLOW_REF looks like
    // `owner/repo/.github/workflows/ci.yml@refs/heads/main`.
    // The SLSA GitHub Actions buildtype spec wants the repo-root-relative
    // path (`.github/workflows/ci.yml`), and the `ref` field wants the
    // portion after `@` — which, for reusable workflows, can differ from
    // `GITHUB_REF` (the caller's triggering ref). So we parse both out of
    // `GITHUB_WORKFLOW_REF` rather than falling back to `GITHUB_REF`.
    let workflow_ref_raw = env("GITHUB_WORKFLOW_REF");
    let (workflow_path, workflow_ref) = parse_workflow_ref(&workflow_ref_raw);

    let run_id = env("GITHUB_RUN_ID");
    let run_attempt = env("GITHUB_RUN_ATTEMPT");
    let invocation_id = if repo_url.is_empty() || run_id.is_empty() {
        String::new()
    } else if run_attempt.is_empty() {
        format!("{repo_url}/actions/runs/{run_id}")
    } else {
        format!("{repo_url}/actions/runs/{run_id}/attempts/{run_attempt}")
    };

    // `builder.id` has to line up with the Fulcio cert's
    // runner-environment extension, so derive it from the same env var
    // we echo into `internalParameters.github.runner_environment`. A
    // self-hosted runner claiming `github-hosted` would be rejected by
    // npm's verifier as a cert/predicate mismatch.
    let runner_environment = env("RUNNER_ENVIRONMENT");
    let builder_id = if runner_environment == "self-hosted" {
        "https://github.com/actions/runner/self-hosted"
    } else {
        "https://github.com/actions/runner/github-hosted"
    };

    let sha = env("GITHUB_SHA");
    // `resolvedDependencies` points at the *triggering* commit, so here
    // we do want GITHUB_REF (the caller's ref), not the workflow ref.
    let git_ref = env("GITHUB_REF");
    let resolved_uri = if repo_url.is_empty() {
        String::new()
    } else {
        format!("git+{repo_url}@{git_ref}")
    };

    serde_json::json!({
        "buildDefinition": {
            "buildType": GITHUB_ACTIONS_BUILD_TYPE,
            "externalParameters": {
                "workflow": {
                    "ref": workflow_ref,
                    "repository": repo_url,
                    "path": workflow_path,
                }
            },
            "internalParameters": {
                "github": {
                    "event_name": env("GITHUB_EVENT_NAME"),
                    "repository_id": env("GITHUB_REPOSITORY_ID"),
                    "repository_owner_id": env("GITHUB_REPOSITORY_OWNER_ID"),
                    "runner_environment": runner_environment,
                }
            },
            "resolvedDependencies": [{
                "uri": resolved_uri,
                "digest": { "gitCommit": sha },
            }],
        },
        "runDetails": {
            "builder": {
                "id": builder_id,
            },
            "metadata": {
                "invocationId": invocation_id,
            },
        },
    })
}

/// Split `GITHUB_WORKFLOW_REF` into `(path, ref)`. The env var has the shape
/// `owner/repo/.github/workflows/ci.yml@refs/heads/main`; we want the
/// repo-root-relative path and the post-`@` ref. Missing pieces come back
/// as empty strings so the caller can splat them into JSON without
/// panicking on bare shells or malformed inputs.
fn parse_workflow_ref(raw: &str) -> (String, String) {
    if raw.is_empty() {
        return (String::new(), String::new());
    }
    let (path_and_owner, git_ref) = raw.split_once('@').unwrap_or((raw, ""));
    // splitn(3, '/') peels off `owner` and `repo`, leaving the workflow
    // path under the third element. If the string has fewer than two
    // slashes it's malformed — fall back to an empty path rather than
    // shipping garbage to the attestation.
    let path = path_and_owner
        .splitn(3, '/')
        .nth(2)
        .unwrap_or("")
        .to_string();
    (path, git_ref.to_string())
}

fn generic_predicate() -> serde_json::Value {
    serde_json::json!({
        "buildDefinition": {
            "buildType": "https://aube.sh/publish/v1",
            "externalParameters": {},
            "internalParameters": {},
            "resolvedDependencies": [],
        },
        "runDetails": {
            "builder": { "id": "https://aube.sh/publish" },
            "metadata": {},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purl_plain_package() {
        assert_eq!(npm_purl("lodash", "4.17.21"), "pkg:npm/lodash@4.17.21");
    }

    #[test]
    fn purl_scoped_package_encodes_only_at_sign() {
        // libnpmpublish encodes `@` → `%40` but leaves `/` intact, which
        // is the canonical purl form for npm scoped packages. Encoding the
        // slash too (via `encodeURIComponent`) would not match what npm's
        // verifier looks up against the registry's dist metadata.
        assert_eq!(
            npm_purl("@scope/foo", "1.0.0"),
            "pkg:npm/%40scope/foo@1.0.0"
        );
    }

    #[test]
    fn generic_predicate_has_slsa_shape() {
        let v = generic_predicate();
        assert!(v.get("buildDefinition").is_some());
        assert!(v.get("runDetails").is_some());
    }

    #[test]
    fn parse_workflow_ref_strips_owner_and_repo() {
        let (path, git_ref) =
            parse_workflow_ref("octocat/hello/.github/workflows/ci.yml@refs/heads/main");
        assert_eq!(path, ".github/workflows/ci.yml");
        assert_eq!(git_ref, "refs/heads/main");
    }

    #[test]
    fn parse_workflow_ref_handles_nested_workflow_dirs() {
        // Reusable workflows can live under an arbitrary sub-path; we only
        // ever strip `owner` and `repo` (the first two segments), so any
        // deeper nesting must survive intact.
        let (path, git_ref) =
            parse_workflow_ref("octocat/hello/subdir/.github/workflows/ci.yml@refs/tags/v1.0.0");
        assert_eq!(path, "subdir/.github/workflows/ci.yml");
        assert_eq!(git_ref, "refs/tags/v1.0.0");
    }

    #[test]
    fn parse_workflow_ref_empty_is_empty() {
        assert_eq!(parse_workflow_ref(""), (String::new(), String::new()));
    }

    #[test]
    fn parse_workflow_ref_missing_at_yields_empty_ref() {
        let (path, git_ref) = parse_workflow_ref("octocat/hello/.github/workflows/ci.yml");
        assert_eq!(path, ".github/workflows/ci.yml");
        assert_eq!(git_ref, "");
    }

    #[test]
    fn github_predicate_reads_env_safely_when_unset() {
        // build_slsa_predicate must never panic even on a bare shell —
        // missing env vars should become empty strings, not crashes.
        let v = github_actions_predicate();
        assert_eq!(
            v["buildDefinition"]["buildType"],
            serde_json::json!(GITHUB_ACTIONS_BUILD_TYPE)
        );
    }
}
