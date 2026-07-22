//! `aube bugs` — open package bug trackers.
//!
//! Mirrors `pnpm bugs`; `issues` is wired as a command alias. Without
//! arguments, the current project's `package.json` supplies `bugs` or
//! `repository`. With package names, metadata is fetched from the registry.

use crate::commands::{make_client, packument_full_cache_dir, resolve_version, split_name_spec};
use clap::Args;
use miette::miette;
use serde_json::Value;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube bugs

  $ aube bugs react

  $ aube issues react react-dom
";

#[derive(Debug, Args)]
pub struct BugsArgs {
    /// Packages to open bug trackers for. Defaults to the current project.
    pub packages: Vec<String>,

    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: BugsArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let urls = if args.packages.is_empty() {
        vec![current_project_url()?]
    } else {
        registry_urls(&args.packages).await?
    };

    for url in urls {
        println!("{url}");
        open_url(&url);
    }
    Ok(())
}

fn current_project_url() -> miette::Result<String> {
    let cwd = crate::dirs::project_root_or_cwd()?;
    let manifest_path = cwd.join("package.json");
    let manifest = aube_manifest::PackageJson::from_path_cached(&manifest_path)
        .map_err(|e| miette!("failed to read {}: {e}", manifest_path.display()))?;
    bug_url_from_map(&manifest.extra).ok_or_else(|| {
        miette!(
            "{} has no `bugs.url` or usable `repository` field",
            manifest_path.display()
        )
    })
}

async fn registry_urls(packages: &[String]) -> miette::Result<Vec<String>> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);
    let mut out = Vec::with_capacity(packages.len());

    for package in packages {
        let (name, version_spec) = split_name_spec(package);
        let packument = client
            .fetch_packument_full_cached(name, &packument_full_cache_dir())
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
        let url = bug_url_from_value(version_meta)
            .or_else(|| bug_url_from_value(&packument))
            .ok_or_else(|| miette!("{name}@{version} has no bugs or repository URL"))?;
        out.push(url);
    }

    Ok(out)
}

fn bug_url_from_map(extra: &std::collections::BTreeMap<String, Value>) -> Option<String> {
    bugs_url_from_extra(extra).or_else(|| repository_issues_url_from_extra(extra))
}

fn bug_url_from_value(value: &Value) -> Option<String> {
    bugs_url(value).or_else(|| repository_issues_url(value))
}

fn bugs_url_from_extra(extra: &std::collections::BTreeMap<String, Value>) -> Option<String> {
    match extra.get("bugs")? {
        Value::String(s) => clean_url(s),
        Value::Object(map) => map.get("url").and_then(Value::as_str).and_then(clean_url),
        _ => None,
    }
}

fn bugs_url(value: &Value) -> Option<String> {
    match value.get("bugs")? {
        Value::String(s) => clean_url(s),
        Value::Object(map) => map.get("url").and_then(Value::as_str).and_then(clean_url),
        _ => None,
    }
}

fn repository_issues_url(value: &Value) -> Option<String> {
    let repo = match value.get("repository")? {
        Value::String(s) => s,
        Value::Object(map) => map.get("url")?.as_str()?,
        _ => return None,
    };
    issues_url_for_repo(repo)
}

fn repository_issues_url_from_extra(
    extra: &std::collections::BTreeMap<String, Value>,
) -> Option<String> {
    let repo = match extra.get("repository")? {
        Value::String(s) => s.as_str(),
        Value::Object(map) => map.get("url")?.as_str()?,
        _ => return None,
    };
    issues_url_for_repo(repo)
}

fn issues_url_for_repo(repo: &str) -> Option<String> {
    let mut url = normalize_repository_url(repo)?;
    if is_complete_issues_url(&url) {
        return Some(url);
    }
    if url.ends_with('/') {
        url.push_str("issues");
    } else {
        url.push_str("/issues");
    }
    Some(url)
}

fn is_complete_issues_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let Some((_, path)) = rest.split_once('/') else {
        return false;
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segments
        .iter()
        .position(|segment| *segment == "issues")
        .is_some_and(|idx| idx >= 2)
}

fn clean_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if is_safe_browser_url(raw) {
        Some(raw.to_string())
    } else {
        None
    }
}

fn normalize_repository_url(raw: &str) -> Option<String> {
    let raw = raw.trim().strip_prefix("git+").unwrap_or(raw.trim());
    if raw.is_empty() {
        return None;
    }
    let raw = raw.split_once('#').map_or(raw, |(base, _)| base);
    let mut url = if let Some(rest) = raw.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("https://{host}/{path}")
    } else if let Some(rest) = raw
        .strip_prefix("ssh://git@")
        .or_else(|| raw.strip_prefix("git://"))
    {
        let (host, path) = rest.split_once('/')?;
        format!("https://{host}/{path}")
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if let Some(path) = raw.strip_prefix("github:") {
        format!("https://github.com/{}", path.trim_matches('/'))
    } else if let Some(path) = raw.strip_prefix("gitlab:") {
        format!("https://gitlab.com/{}", path.trim_matches('/'))
    } else if let Some(path) = raw.strip_prefix("bitbucket:") {
        format!("https://bitbucket.org/{}", path.trim_matches('/'))
    } else if raw.split('/').count() == 2 && !raw.contains(':') {
        format!("https://github.com/{}", raw.trim_matches('/'))
    } else {
        return clean_url(raw);
    };
    if let Some(stripped) = url.strip_suffix(".git") {
        url = stripped.to_string();
    }
    clean_url(&url)
}

fn open_url(url: &str) {
    if std::env::var_os("AUBE_NO_OPEN").is_some() {
        return;
    }
    if !is_safe_browser_url(url) {
        tracing::debug!("refusing to open unsafe URL: {url:?}");
        return;
    }
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        let escaped = url.replace('%', "%%");
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &escaped])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if let Err(e) = result {
        tracing::debug!("failed to open {url}: {e}");
    }
}

fn is_safe_browser_url(url: &str) -> bool {
    let rest = match url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    if rest.is_empty() || rest.len() > 2048 {
        return false;
    }
    rest.chars().all(|c| {
        matches!(c,
            'a'..='z' | 'A'..='Z' | '0'..='9'
            | '-' | '_' | '.' | '~'
            | ':' | '/' | '?' | '#' | '[' | ']' | '@'
            | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ','
            | ';' | '=' | '%'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bug_url_prefers_bugs_url() {
        let value = serde_json::json!({
            "bugs": { "url": "https://github.com/acme/pkg/issues" },
            "repository": "github:other/pkg"
        });
        assert_eq!(
            bug_url_from_value(&value).as_deref(),
            Some("https://github.com/acme/pkg/issues")
        );
    }

    #[test]
    fn bug_url_accepts_string_bugs_field() {
        let value = serde_json::json!({
            "bugs": "https://bugs.example.com/pkg"
        });
        assert_eq!(
            bug_url_from_value(&value).as_deref(),
            Some("https://bugs.example.com/pkg")
        );
    }

    #[test]
    fn bug_url_falls_back_to_repository_issues() {
        for (repo, expected) in [
            ("github:acme/pkg.git", "https://github.com/acme/pkg/issues"),
            ("acme/pkg", "https://github.com/acme/pkg/issues"),
            (
                "https://github.com/acme/issues",
                "https://github.com/acme/issues/issues",
            ),
            (
                "https://github.com/acme/pkg/issues",
                "https://github.com/acme/pkg/issues",
            ),
            (
                "https://github.com/acme/pkg/issues/123",
                "https://github.com/acme/pkg/issues/123",
            ),
            (
                "git+https://github.com/acme/pkg.git#main",
                "https://github.com/acme/pkg/issues",
            ),
            (
                "git@gitlab.com:acme/pkg.git",
                "https://gitlab.com/acme/pkg/issues",
            ),
            (
                "git+ssh://git@github.com/acme/pkg.git",
                "https://github.com/acme/pkg/issues",
            ),
            (
                "git://github.com/acme/pkg.git",
                "https://github.com/acme/pkg/issues",
            ),
        ] {
            let value = serde_json::json!({ "repository": repo });
            assert_eq!(bug_url_from_value(&value).as_deref(), Some(expected));
        }
    }

    #[test]
    fn bug_url_rejects_email_only_bugs_field() {
        let value = serde_json::json!({
            "bugs": { "email": "bugs@example.com" }
        });
        assert_eq!(bug_url_from_value(&value), None);
    }

    #[test]
    fn bug_url_rejects_unsafe_browser_urls() {
        for url in [
            "javascript:alert(1)",
            "https://example.com/bug|calc.exe",
            "https://example.com/bug^calc.exe",
            "https://example.com/bug with space",
        ] {
            let value = serde_json::json!({ "bugs": { "url": url } });
            assert_eq!(bug_url_from_value(&value), None, "{url} should be rejected");
        }
    }
}
