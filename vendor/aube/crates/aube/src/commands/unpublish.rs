//! `aube unpublish` — remove a package (or a single version of it) from
//! a registry.
//!
//! Mirrors the pnpm/npm surface:
//!
//! - `aube unpublish` — reads `./package.json` and unpublishes the
//!   specific `name@version` it names (single-version path).
//! - `aube unpublish <name>@<version>` — single-version unpublish of an
//!   arbitrary package.
//! - `aube unpublish <name>` — unpublishes the *entire* package. This is
//!   destructive: every version and every tag goes away. npm/pnpm
//!   require `--force` here, and so do we.
//!
//! The single-version flow is a two-step dance the npm registry requires:
//!
//!   1. `GET /{name}` to fetch the full packument and its current `_rev`.
//!   2. Edit the packument in memory: drop the doomed version from
//!      `versions`, `time`, and any `dist-tags` pointing at it.
//!   3. `PUT /{name}/-rev/{rev}` with the trimmed packument. The registry
//!      returns a new `_rev`.
//!   4. `DELETE /{name}/-/{tarball}/-rev/{new_rev}` to evict the file.
//!
//! The whole-package flow is a single `DELETE /{name}/-rev/{rev}`.
//!
//! Auth and per-registry TLS follow the same `RegistryClient` path as
//! `aube publish` / `aube dist-tag`.

use crate::commands::{encode_package_name, ensure_registry_auth_for_package, split_name_spec};
use aube_manifest::PackageJson;
use aube_registry::client::RegistryClient;
use aube_registry::config::{NpmConfig, normalize_registry_url_pub};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use serde_json::Value;

#[derive(Debug, Args)]
pub struct UnpublishArgs {
    /// Don't talk to the registry; print what the command would do.
    #[arg(long)]
    pub dry_run: bool,
    /// Required for whole-package unpublish (no `@version` in the spec).
    ///
    /// Single-version unpublish works without it — matching npm,
    /// which is more permissive about dropping one version than
    /// nuking every version in one call.
    #[arg(short, long)]
    pub force: bool,
    /// One-time password for registries that require 2FA.
    ///
    /// Sent verbatim as the `npm-otp` header.
    #[arg(long, value_name = "CODE")]
    pub otp: Option<String>,
    /// Package spec: `name`, `name@version`, or omitted to use the
    /// current project's `package.json`.
    pub spec: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

/// What the user is asking to unpublish, once the optional spec has been
/// collapsed with `./package.json`. Splitting this out keeps the rest of
/// `run` free of `Option` juggling.
struct Target {
    name: String,
    /// `Some(v)` → single-version unpublish; `None` → whole package.
    version: Option<String>,
}

pub async fn run(args: UnpublishArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = if args.spec.is_some() {
        crate::dirs::project_root_or_cwd()?
    } else {
        crate::dirs::project_root()?
    };
    let target = resolve_target(args.spec.as_deref(), &cwd)?;

    if target.version.is_none() && !args.force {
        return Err(miette!(
            "unpublishing an entire package requires --force\nhelp: pass `--force` to drop every version of `{}`, or specify a version (`{}@<version>`)",
            target.name,
            target.name,
        ));
    }

    let config = NpmConfig::load(&cwd);
    let registry_url = args
        .network
        .registry
        .as_deref()
        .map(normalize_registry_url_pub)
        .unwrap_or_else(|| config.registry_for(&target.name).to_string());

    if args.dry_run {
        match &target.version {
            Some(v) => println!(
                "- {}@{} (dry run, would unpublish from {registry_url})",
                target.name, v
            ),
            None => println!(
                "- {} (dry run, would unpublish ALL versions from {registry_url})",
                target.name
            ),
        }
        return Ok(());
    }

    let policy = crate::commands::resolve_fetch_policy(&cwd);
    let client = RegistryClient::from_config_with_policy(config, policy);
    ensure_registry_auth_for_package(&client, &registry_url, &target.name)?;

    match target.version {
        Some(version) => {
            unpublish_version(
                &client,
                &registry_url,
                &target.name,
                &version,
                args.otp.as_deref(),
            )
            .await?;
            println!("- {}@{}", target.name, version);
        }
        None => {
            unpublish_package(&client, &registry_url, &target.name, args.otp.as_deref()).await?;
            println!("- {}", target.name);
        }
    }
    Ok(())
}

/// Turn the optional CLI spec into a concrete target. The three cases
/// correspond to the three shapes `npm unpublish` accepts.
fn resolve_target(spec: Option<&str>, cwd: &std::path::Path) -> miette::Result<Target> {
    if let Some(spec) = spec {
        let (name, version) = split_name_spec(spec);
        if name.is_empty() {
            return Err(miette!("package name is empty in `{spec}`"));
        }
        return Ok(Target {
            name: name.to_string(),
            version: version.filter(|v| !v.is_empty()).map(|v| v.to_string()),
        });
    }
    // No spec: fall back to the current project's manifest. This mirrors
    // `npm unpublish` with no args, which unpublishes the *version*
    // named in `./package.json` rather than the whole package.
    let manifest = PackageJson::from_path(&cwd.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read ./package.json")?;
    let name = manifest
        .name
        .ok_or_else(|| miette!("package.json has no `name` field"))?;
    let version = manifest
        .version
        .ok_or_else(|| miette!("package.json has no `version` field"))?;
    Ok(Target {
        name,
        version: Some(version),
    })
}

/// Whole-package unpublish: fetch `_rev`, then `DELETE /{name}/-rev/{rev}`.
async fn unpublish_package(
    client: &RegistryClient,
    registry_url: &str,
    name: &str,
    otp: Option<&str>,
) -> miette::Result<()> {
    let packument = fetch_packument(client, registry_url, name).await?;
    let rev = extract_rev(&packument, name)?;

    let url = format!(
        "{}/{}/-rev/{}",
        registry_url.trim_end_matches('/'),
        encode_package_name(name),
        rev
    );
    let mut req =
        client.authed_request_for_package(reqwest::Method::DELETE, &url, registry_url, name);
    if let Some(otp) = otp {
        req = req.header("npm-otp", otp);
    }
    let resp = req
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to DELETE {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(miette!("unpublish failed: {status}: {}", body.trim()));
    }
    Ok(())
}

/// Single-version unpublish: trim the packument, PUT it back, then DELETE
/// the version's tarball. See the module-level doc comment for why this
/// is two round trips.
async fn unpublish_version(
    client: &RegistryClient,
    registry_url: &str,
    name: &str,
    version: &str,
    otp: Option<&str>,
) -> miette::Result<()> {
    let mut packument = fetch_packument(client, registry_url, name).await?;
    let rev = extract_rev(&packument, name)?;

    let tarball = strip_version(&mut packument, name, version)?;

    let put_url = format!(
        "{}/{}/-rev/{}",
        registry_url.trim_end_matches('/'),
        encode_package_name(name),
        rev
    );
    let mut req = client
        .authed_request_for_package(reqwest::Method::PUT, &put_url, registry_url, name)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&packument).into_diagnostic()?);
    if let Some(otp) = otp {
        req = req.header("npm-otp", otp);
    }
    let resp = req
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to PUT {put_url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(miette!(
            "unpublish (trim versions) failed: {status}: {}",
            body.trim()
        ));
    }

    // Some registries stop here and garbage-collect the tarball
    // themselves. Others (Verdaccio, Nexus, npmjs) expect an explicit
    // DELETE for the tarball. If the PUT already removed the version
    // and the subsequent DELETE 404s, treat it as success rather than
    // bubbling up a confusing error — the version is gone either way.
    let Some(tarball_path) = tarball else {
        return Ok(());
    };

    // npm-compatible registries return the updated `_rev` in the PUT
    // response body as `{"ok":true,"id":"<name>","rev":"<new_rev>"}`.
    // Parse it directly instead of issuing another GET — saves a round
    // trip on every single-version unpublish. The fallback path covers
    // both "body isn't JSON at all" (some older or proxied registries
    // return an empty/plain-text 2xx) and "body parsed but has no
    // `rev`" — in either case we issue a fresh GET rather than fail,
    // since the PUT itself already succeeded.
    let put_body_bytes = resp
        .bytes()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read PUT response for {name}"))?;
    let new_rev = match serde_json::from_slice::<Value>(&put_body_bytes)
        .ok()
        .as_ref()
        .and_then(|b| b.get("rev"))
        .and_then(Value::as_str)
    {
        Some(rev) => rev.to_string(),
        None => fetch_rev(client, registry_url, name).await?,
    };
    let del_url = format!(
        "{}/{}/-/{}/-rev/{}",
        registry_url.trim_end_matches('/'),
        encode_package_name(name),
        tarball_path,
        new_rev
    );
    let mut req =
        client.authed_request_for_package(reqwest::Method::DELETE, &del_url, registry_url, name);
    if let Some(otp) = otp {
        req = req.header("npm-otp", otp);
    }
    let resp = req
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to DELETE {del_url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(miette!(
            "unpublish (tarball delete) failed: {status}: {}",
            body.trim()
        ));
    }
    Ok(())
}

/// GET the packument with auth. The response body is parsed as raw JSON
/// (not `Packument`) because we need to round-trip fields the strongly
/// typed struct doesn't model (e.g. `_rev`, `time`, `_attachments`).
async fn fetch_packument(
    client: &RegistryClient,
    registry_url: &str,
    name: &str,
) -> miette::Result<Value> {
    let url = format!(
        "{}/{}",
        registry_url.trim_end_matches('/'),
        encode_package_name(name)
    );
    let resp = client
        .authed_request_for_package(reqwest::Method::GET, &url, registry_url, name)
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to GET {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(miette!("package not found: {name}"));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(miette!("failed to fetch {name}: {status}: {}", body.trim()));
    }
    resp.json::<Value>()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse packument for {name}"))
}

async fn fetch_rev(
    client: &RegistryClient,
    registry_url: &str,
    name: &str,
) -> miette::Result<String> {
    let packument = fetch_packument(client, registry_url, name).await?;
    extract_rev(&packument, name)
}

fn extract_rev(packument: &Value, name: &str) -> miette::Result<String> {
    packument
        .get("_rev")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| miette!("packument for {name} has no `_rev` field"))
}

/// Remove `version` from the packument in place. Drops the entry from
/// `versions`, `time`, and any `dist-tags` that point at it. Returns the
/// tarball path (filename without host) so the caller can DELETE it, or
/// `None` if the packument doesn't record one.
fn strip_version(
    packument: &mut Value,
    name: &str,
    version: &str,
) -> miette::Result<Option<String>> {
    let obj = packument
        .as_object_mut()
        .ok_or_else(|| miette!("packument for {name} is not a JSON object"))?;

    let versions = obj
        .get_mut("versions")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| miette!("packument for {name} has no `versions` map"))?;
    let doomed = versions.remove(version).ok_or_else(|| {
        miette!("version {version} is not published for {name} — nothing to unpublish")
    })?;

    // Extract the tarball filename from the outgoing version entry.
    // Registries return the full URL; we only need the last path
    // segment because the DELETE endpoint takes a bare filename. A
    // defensive `?`/`#` split guards against proxies that append a
    // signed-URL query string (not used by npmjs or Verdaccio today,
    // but cheap insurance against a malformed DELETE URL).
    let tarball = doomed
        .get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(Value::as_str)
        .and_then(|url| url.rsplit('/').next())
        .map(|seg| seg.split(['?', '#']).next().unwrap_or(seg).to_string());

    if let Some(time) = obj.get_mut("time").and_then(Value::as_object_mut) {
        time.remove(version);
    }
    // Remaining version numbers, for the `latest` reassignment below.
    // Collected *before* we touch `dist-tags` so the borrow checker is
    // happy and so we only see survivors.
    let survivors: Vec<String> = obj
        .get("versions")
        .and_then(Value::as_object)
        .map(|v| v.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(tags) = obj.get_mut("dist-tags").and_then(Value::as_object_mut) {
        // Drop every tag pointing at the doomed version.
        tags.retain(|_, v| v.as_str() != Some(version));
        // If `latest` was one of those tags, reassign it to the highest
        // remaining semver. Without this the package can end up in a
        // state where `npm install <name>` fails or silently picks an
        // arbitrary version — npm's own CLI does this reassignment so
        // we match the behavior on Verdaccio/Nexus/private registries
        // that don't auto-repair the tag.
        if !tags.contains_key("latest")
            && let Some(highest) = highest_semver(&survivors)
        {
            tags.insert("latest".to_string(), Value::String(highest));
        }
    }
    Ok(tarball)
}

/// Pick the highest semver from a list of raw version strings, ignoring
/// anything that doesn't parse. Returns `None` if the list is empty or
/// nothing parses. Used to reassign `latest` when the unpublish drops
/// the version it used to point at.
fn highest_semver(versions: &[String]) -> Option<String> {
    versions
        .iter()
        .filter_map(|v| {
            node_semver::Version::parse(v)
                .ok()
                .map(|parsed| (parsed, v))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, raw)| raw.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highest_semver_picks_max() {
        let v = vec![
            "1.2.3".to_string(),
            "2.0.0-rc.1".to_string(),
            "1.9.0".to_string(),
        ];
        // 2.0.0-rc.1 is a pre-release; 1.9.0 is the highest stable.
        // semver ordering ranks a pre-release lower than the corresponding
        // stable, but higher than all prior stables — so 2.0.0-rc.1 wins
        // over 1.9.0, matching what `npm dist-tag` would pick.
        assert_eq!(highest_semver(&v).as_deref(), Some("2.0.0-rc.1"));
    }

    #[test]
    fn highest_semver_ignores_unparseable() {
        let v = vec!["not-a-version".to_string(), "1.0.0".to_string()];
        assert_eq!(highest_semver(&v).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn highest_semver_empty_is_none() {
        assert!(highest_semver(&[]).is_none());
    }

    #[test]
    fn strip_version_reassigns_latest_to_highest_remaining() {
        let mut pkg = serde_json::json!({
            "_rev": "3-abc",
            "name": "demo",
            "dist-tags": { "latest": "2.0.0", "next": "3.0.0-rc.1" },
            "time": {},
            "versions": {
                "1.0.0": { "dist": { "tarball": "https://r.test/demo/-/demo-1.0.0.tgz" } },
                "1.9.0": { "dist": { "tarball": "https://r.test/demo/-/demo-1.9.0.tgz" } },
                "2.0.0": { "dist": { "tarball": "https://r.test/demo/-/demo-2.0.0.tgz" } },
                "3.0.0-rc.1": { "dist": { "tarball": "https://r.test/demo/-/demo-3.0.0-rc.1.tgz" } }
            }
        });
        strip_version(&mut pkg, "demo", "2.0.0").unwrap();
        // `latest` was pointing at 2.0.0 — should reassign to the
        // highest remaining, which is 3.0.0-rc.1 (a pre-release still
        // ranks above 1.9.0).
        assert_eq!(pkg["dist-tags"]["latest"], "3.0.0-rc.1");
        // `next` is untouched because it wasn't pointing at 2.0.0.
        assert_eq!(pkg["dist-tags"]["next"], "3.0.0-rc.1");
    }

    #[test]
    fn strip_version_drops_latest_when_no_survivors() {
        // Single-version package: removing the only version leaves
        // nothing for `latest` to point at, so the tag should be gone.
        let mut pkg = serde_json::json!({
            "_rev": "1-abc",
            "dist-tags": { "latest": "1.0.0" },
            "versions": {
                "1.0.0": { "dist": { "tarball": "https://r.test/demo/-/demo-1.0.0.tgz" } }
            }
        });
        strip_version(&mut pkg, "demo", "1.0.0").unwrap();
        assert!(pkg["dist-tags"].get("latest").is_none());
    }

    #[test]
    fn strip_version_tarball_query_string_is_stripped() {
        // Guard against proxies that append a signed-URL query string
        // to the tarball field. We only want the bare filename.
        let mut pkg = serde_json::json!({
            "_rev": "1-abc",
            "dist-tags": {},
            "versions": {
                "1.0.0": { "dist": { "tarball": "https://r.test/demo/-/demo-1.0.0.tgz?sig=abc&exp=123" } }
            }
        });
        let tarball = strip_version(&mut pkg, "demo", "1.0.0").unwrap();
        assert_eq!(tarball.as_deref(), Some("demo-1.0.0.tgz"));
    }

    #[test]
    fn resolve_target_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"demo","version":"1.2.3"}"#,
        )
        .unwrap();
        let t = resolve_target(None, dir.path()).unwrap();
        assert_eq!(t.name, "demo");
        assert_eq!(t.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn resolve_target_name_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = resolve_target(Some("lodash"), dir.path()).unwrap();
        assert_eq!(t.name, "lodash");
        assert!(t.version.is_none());
    }

    #[test]
    fn resolve_target_name_version() {
        let dir = tempfile::tempdir().unwrap();
        let t = resolve_target(Some("lodash@4.17.21"), dir.path()).unwrap();
        assert_eq!(t.name, "lodash");
        assert_eq!(t.version.as_deref(), Some("4.17.21"));
    }

    #[test]
    fn resolve_target_scoped_name_version() {
        let dir = tempfile::tempdir().unwrap();
        let t = resolve_target(Some("@scope/pkg@0.1.0"), dir.path()).unwrap();
        assert_eq!(t.name, "@scope/pkg");
        assert_eq!(t.version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn strip_version_drops_versions_time_and_tags() {
        let mut pkg = serde_json::json!({
            "_rev": "3-abc",
            "name": "demo",
            "dist-tags": { "latest": "1.2.3", "next": "2.0.0-rc.1" },
            "time": { "1.2.3": "2024-01-01", "2.0.0-rc.1": "2024-06-01" },
            "versions": {
                "1.2.3": { "dist": { "tarball": "https://r.test/demo/-/demo-1.2.3.tgz" } },
                "2.0.0-rc.1": { "dist": { "tarball": "https://r.test/demo/-/demo-2.0.0-rc.1.tgz" } }
            }
        });
        let tarball = strip_version(&mut pkg, "demo", "1.2.3").unwrap();
        assert_eq!(tarball.as_deref(), Some("demo-1.2.3.tgz"));
        // Version is gone.
        assert!(pkg["versions"].get("1.2.3").is_none());
        // `time` entry is gone.
        assert!(pkg["time"].get("1.2.3").is_none());
        // `latest` pointed at 1.2.3; reassigns to the highest surviving
        // version (2.0.0-rc.1 — the only one left). `next` is
        // untouched because it wasn't pointing at the doomed version.
        assert_eq!(pkg["dist-tags"]["latest"], "2.0.0-rc.1");
        assert_eq!(pkg["dist-tags"]["next"], "2.0.0-rc.1");
    }

    #[test]
    fn strip_version_errors_when_version_missing() {
        let mut pkg = serde_json::json!({
            "_rev": "1-x",
            "versions": { "1.0.0": {} }
        });
        let err = strip_version(&mut pkg, "demo", "9.9.9").unwrap_err();
        assert!(err.to_string().contains("not published"));
    }

    #[test]
    fn extract_rev_returns_value() {
        let pkg = serde_json::json!({"_rev": "7-deadbeef"});
        assert_eq!(extract_rev(&pkg, "demo").unwrap(), "7-deadbeef");
    }

    #[test]
    fn extract_rev_errors_when_missing() {
        let pkg = serde_json::json!({});
        assert!(extract_rev(&pkg, "demo").is_err());
    }
}
