use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use toml::Value;

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";

#[derive(Debug, Default)]
pub(crate) struct BunfigNpmrcEntries {
    pub(crate) user: Vec<(String, String)>,
    pub(crate) project: Vec<(String, String)>,
}

/// Load the limited Bun config subset Nub can map into the engine's existing
/// `.npmrc`-shaped settings model. This is called only after the PM surface has
/// been proven Bun-incumbent.
pub(crate) fn load_bunfig_npmrc_entries(project_root: &Path) -> BunfigNpmrcEntries {
    let mut entries = BunfigNpmrcEntries::default();
    if let Some(path) = global_bunfig_path() {
        entries.user.extend(load_bunfig_file(&path));
    }
    entries
        .project
        .extend(load_bunfig_file(&project_root.join("bunfig.toml")));
    entries
}

fn global_bunfig_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .map(|dir| dir.join(".bunfig.toml"))
}

fn load_bunfig_file(path: &Path) -> Vec<(String, String)> {
    parse_bunfig_file(path)
        .map(|parsed| entries_from_bunfig(&parsed))
        .unwrap_or_default()
}

fn parse_bunfig_file(path: &Path) -> Option<Value> {
    let table = std::fs::read_to_string(path).ok()?.parse::<toml::Table>().ok()?;
    Some(Value::Table(table))
}

/// Whether the project's `bunfig.toml` directs `node_modules` layout.
/// `entries_from_bunfig` deliberately never maps `[install].linker`, so the
/// install header asks this to point at `nub.jsonc` instead of dropping the key
/// in silence.
pub(crate) fn declares_install_linker(project_root: &Path) -> bool {
    parse_bunfig_file(&project_root.join("bunfig.toml")).is_some_and(|root| {
        root.get("install")
            .and_then(Value::as_table)
            .is_some_and(|install| install.contains_key("linker"))
    })
}

fn entries_from_bunfig(root: &Value) -> Vec<(String, String)> {
    let Some(install) = root.get("install").and_then(Value::as_table) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let default_registry = install.get("registry").and_then(parse_registry);
    let default_url = default_registry
        .as_ref()
        .and_then(BunRegistry::explicit_url)
        .unwrap_or(DEFAULT_REGISTRY)
        .to_string();
    let normalized_default_url = aube_registry::config::normalize_registry_url_pub(&default_url);
    if let Some(registry) = default_registry {
        push_registry_entries(&mut out, None, registry, DEFAULT_REGISTRY, true);
    }
    if let Some(scopes) = install.get("scopes").and_then(Value::as_table) {
        let scoped_registries: Vec<_> = scopes
            .iter()
            .filter_map(|(name, value)| {
                let scope = name.strip_prefix('@').unwrap_or(name);
                (!scope.is_empty())
                    .then(|| parse_registry(value).map(|registry| (scope.to_string(), registry)))
                    .flatten()
            })
            .collect();
        let auth_registry_counts = scope_auth_registry_counts(&scoped_registries);
        for (scope, registry) in scoped_registries {
            let explicit_url = registry
                .explicit_url()
                .map(aube_registry::config::normalize_registry_url_pub);
            let scope_auth_is_representable = explicit_url.as_ref().is_some_and(|url| {
                url != &normalized_default_url
                    && auth_registry_counts.get(url).copied().unwrap_or(0) == 1
            });
            push_registry_entries(
                &mut out,
                Some(&scope),
                registry,
                &default_url,
                scope_auth_is_representable,
            );
        }
    }
    // `[install].linker` is deliberately not mapped. Nub cannot reproduce Bun's
    // linker modes from that branded setting; a Bun-owned project uses the
    // neutral `.npmrc` key or CLI flag instead. Every other key below is
    // resolution or network config that the compat layer mirrors.
    //
    // Bun's supply-chain age gate: `[install].minimumReleaseAge` → the engine's
    // `minimumReleaseAge` setting (same gate nub reads from `.npmrc`/pnpm
    // config). Without this a bun user's in-bunfig hardening is silently
    // dropped. UNITS DIFFER: bunfig is in SECONDS (bun's
    // `bunfig.zig`: `minimum_release_age_ms = seconds * ms_per_s`; the error
    // is "Expected number of seconds"), while the engine setting is in MINUTES
    // (settings.toml `[minimumReleaseAge]` "(minutes)"; resolver error "older
    // than N minute(s)"). Convert seconds → minutes (÷60), rounding UP so a
    // small non-zero bunfig value never collapses to 0 and silently disables
    // the gate. `minimumReleaseAgeExcludes` (bun's spelling) maps to the
    // engine's `minimumReleaseAgeExclude`.
    if let Some(seconds) = install
        .get("minimumReleaseAge")
        .and_then(Value::as_integer)
        .filter(|n| *n >= 0)
    {
        // Ceiling division: a 30s gate maps to 1 minute, not 0 (and 0s → 0).
        // (signed `i64::div_ceil` is still unstable, so spell it out.)
        let minutes = (seconds + 59) / 60;
        out.push(("minimumReleaseAge".to_string(), minutes.to_string()));
    }
    if let Some(excludes) = install
        .get("minimumReleaseAgeExcludes")
        .and_then(Value::as_array)
    {
        let names: Vec<String> = excludes
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !names.is_empty() {
            out.push(("minimumReleaseAgeExclude".to_string(), names.join(",")));
        }
    }
    push_tls_entries(&mut out, install);
    out
}

/// Map bunfig's `[install] cafile` / `ca` onto the unscoped npmrc TLS keys the
/// engine already wires into its rustls client (`cafile` and `ca`/`ca[]`).
/// Mirrors Bun's `cli/bunfig.zig`: `cafile` is a path to a PEM file; `ca` is
/// either a single inline PEM string or an array of them.
fn push_tls_entries(out: &mut Vec<(String, String)>, install: &toml::map::Map<String, Value>) {
    if let Some(cafile) = install
        .get("cafile")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        out.push(("cafile".to_string(), cafile.to_string()));
    }
    match install.get("ca") {
        Some(Value::String(ca)) if !ca.is_empty() => {
            out.push(("ca".to_string(), ca.clone()));
        }
        Some(Value::Array(items)) => {
            // npm/pnpm build up the trust list from repeated `ca[]=...` lines;
            // emit one per inline PEM so the engine's `ca`/`ca[]` apply arm
            // pushes them all.
            for item in items {
                if let Some(pem) = item.as_str().filter(|s| !s.is_empty()) {
                    out.push(("ca[]".to_string(), pem.to_string()));
                }
            }
        }
        _ => {}
    }
}

fn scope_auth_registry_counts(scopes: &[(String, BunRegistry)]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for (_, registry) in scopes {
        if !registry.has_auth() {
            continue;
        }
        if let Some(url) = registry
            .explicit_url()
            .map(aube_registry::config::normalize_registry_url_pub)
        {
            *counts.entry(url).or_insert(0) += 1;
        }
    }
    counts
}

impl BunRegistry {
    fn explicit_url(&self) -> Option<&str> {
        self.url.as_deref().filter(|url| !url.is_empty())
    }

    fn has_auth(&self) -> bool {
        self.token.as_deref().is_some_and(|v| !v.is_empty())
            || (self.username.as_deref().is_some_and(|v| !v.is_empty())
                && self.password.as_deref().is_some_and(|v| !v.is_empty()))
    }
}

#[derive(Debug, Default)]
struct BunRegistry {
    url: Option<String>,
    token: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

fn parse_registry(value: &Value) -> Option<BunRegistry> {
    if let Some(raw) = value.as_str() {
        return Some(parse_registry_string(raw));
    }
    let table = value.as_table()?;
    let str_field = |key| {
        table
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    };
    Some(BunRegistry {
        url: str_field("url"),
        token: str_field("token"),
        username: str_field("username"),
        password: str_field("password"),
    })
}

fn parse_registry_string(raw: &str) -> BunRegistry {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return BunRegistry {
            url: Some(raw.to_string()),
            ..BunRegistry::default()
        };
    };
    let username = parsed.username().to_string();
    let password = parsed.password().map(ToOwned::to_owned);
    if username.is_empty() && password.is_none() {
        return BunRegistry {
            url: Some(raw.to_string()),
            ..BunRegistry::default()
        };
    }
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    let url = Some(parsed.to_string());
    match (username.is_empty(), password) {
        (true, Some(token)) if !token.is_empty() => BunRegistry {
            url,
            token: Some(token),
            ..BunRegistry::default()
        },
        (false, Some(password)) if !password.is_empty() => BunRegistry {
            url,
            username: Some(username),
            password: Some(password),
            ..BunRegistry::default()
        },
        _ => BunRegistry {
            url: Some(raw.to_string()),
            ..BunRegistry::default()
        },
    }
}

fn push_registry_entries(
    out: &mut Vec<(String, String)>,
    scope: Option<&str>,
    registry: BunRegistry,
    fallback_url: &str,
    include_auth: bool,
) {
    let BunRegistry {
        url,
        token,
        username,
        password,
    } = registry;
    let (url, has_explicit_url) = match url {
        Some(url) if !url.is_empty() => (url, true),
        _ => (fallback_url.to_string(), false),
    };
    if url.is_empty() {
        return;
    };
    match scope {
        Some(scope) => out.push((format!("@{scope}:registry"), url.clone())),
        None if has_explicit_url => out.push(("registry".to_string(), url.clone())),
        None => {}
    }
    if !include_auth {
        return;
    }
    let uri = aube_registry::config::registry_uri_key_pub(&url);
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        out.push((format!("{uri}:_authToken"), token));
    }
    if let (Some(username), Some(password)) = (username, password)
        && !username.is_empty()
        && !password.is_empty()
    {
        let raw = format!("{username}:{password}");
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        out.push((format!("{uri}:_auth"), encoded));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> Vec<(String, String)> {
        let value = Value::Table(source.parse::<toml::Table>().unwrap());
        entries_from_bunfig(&value)
    }

    #[test]
    fn maps_registry_and_scope_auth_but_never_the_layout_linker() {
        let entries = parsed(
            r#"
            [install]
            registry = { url = "https://registry.example.com/", token = "tok" }
            linker = "isolated"

            [install.scopes]
            "@acme" = { url = "https://npm.pkg.example.com/", username = "u", password = "p" }
            plain = "https://plain.example.com/"
            "#,
        );

        assert!(entries.contains(&(
            "registry".to_string(),
            "https://registry.example.com/".to_string()
        )));
        assert!(entries.contains(&(
            "//registry.example.com/:_authToken".to_string(),
            "tok".to_string()
        )));
        assert!(entries.contains(&(
            "@acme:registry".to_string(),
            "https://npm.pkg.example.com/".to_string()
        )));
        assert!(
            entries
                .iter()
                .any(|(k, _)| k == "//npm.pkg.example.com/:_auth")
        );
        assert!(entries.contains(&(
            "@plain:registry".to_string(),
            "https://plain.example.com/".to_string()
        )));
        assert!(
            entries.iter().all(|(k, _)| k != "nodeLinker"),
            "bunfig `linker` is layout direction and must not reach the engine: {entries:?}"
        );
    }

    #[test]
    fn converts_minimum_release_age_seconds_to_engine_minutes_and_maps_excludes() {
        // bunfig is in SECONDS (bun docs' example: `259200 # seconds` = 3 days);
        // the engine setting is in MINUTES. 259200s ÷ 60 = 4320 minutes (3 days),
        // NOT 259200 of the engine's unit (which would be a 60× over-aggressive
        // 180-day gate).
        let entries = parsed(
            r#"
            [install]
            minimumReleaseAge = 259200
            minimumReleaseAgeExcludes = ["@acme/internal", "trusted-pkg"]
            "#,
        );

        assert!(
            entries.contains(&("minimumReleaseAge".to_string(), "4320".to_string())),
            "expected 259200 seconds to convert to 4320 minutes, got {entries:?}"
        );
        assert!(entries.contains(&(
            "minimumReleaseAgeExclude".to_string(),
            "@acme/internal,trusted-pkg".to_string()
        )));
    }

    #[test]
    fn minimum_release_age_rounds_up_so_small_gates_never_disable() {
        // A 30-second gate must not collapse to 0 minutes (which disables the
        // gate); ceiling division yields 1 minute.
        let entries = parsed("[install]\nminimumReleaseAge = 30\n");
        assert!(
            entries.contains(&("minimumReleaseAge".to_string(), "1".to_string())),
            "expected 30 seconds to round up to 1 minute, got {entries:?}"
        );

        // An explicit 0 stays 0 (gate disabled, matching bun).
        let zero = parsed("[install]\nminimumReleaseAge = 0\n");
        assert!(zero.contains(&("minimumReleaseAge".to_string(), "0".to_string())));
    }

    #[test]
    fn maps_cafile_and_inline_ca_string() {
        let entries = parsed(
            r#"
            [install]
            cafile = "/etc/ssl/corp-ca.pem"
            ca = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----"
            "#,
        );

        assert!(entries.contains(&("cafile".to_string(), "/etc/ssl/corp-ca.pem".to_string())));
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "ca" && v.contains("BEGIN CERTIFICATE"))
        );
    }

    #[test]
    fn maps_inline_ca_array_to_repeated_ca_entries() {
        let entries = parsed(
            r#"
            [install]
            ca = ["-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----", "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----"]
            "#,
        );

        let ca_entries: Vec<_> = entries.iter().filter(|(k, _)| k == "ca[]").collect();
        assert_eq!(
            ca_entries.len(),
            2,
            "each inline PEM becomes one ca[] entry"
        );
        assert!(ca_entries.iter().any(|(_, v)| v.contains("AAAA")));
        assert!(ca_entries.iter().any(|(_, v)| v.contains("BBBB")));
    }

    #[test]
    fn maps_token_in_registry_url_like_bun() {
        let entries = parsed(
            r#"
            [install]
            registry = "https://:tok@registry.example.com/"
            "#,
        );

        assert!(entries.contains(&(
            "registry".to_string(),
            "https://registry.example.com/".to_string()
        )));
        assert!(entries.contains(&(
            "//registry.example.com/:_authToken".to_string(),
            "tok".to_string()
        )));
    }

    #[test]
    fn maps_url_less_registry_auth_and_leaves_url_less_scope_auth_unprojected() {
        let entries = parsed(
            r#"
            [install]
            registry = { token = "tok" }

            [install.scopes]
            "@acme" = { username = "u", password = "p" }
            "#,
        );

        assert!(!entries.iter().any(|(key, _)| key == "registry"));
        assert!(entries.contains(&(
            "//registry.npmjs.org/:_authToken".to_string(),
            "tok".to_string()
        )));
        assert!(entries.contains(&(
            "@acme:registry".to_string(),
            "https://registry.npmjs.org/".to_string()
        )));
        assert!(
            !entries
                .iter()
                .any(|(key, _)| key == "//registry.npmjs.org/:_auth"),
            "URL-less scoped auth inherits the default registry, but npmrc cannot keep it scope-local"
        );
    }

    #[test]
    fn maps_url_less_scope_auth_to_registry_without_widening_auth() {
        let entries = parsed(
            r#"
            [install]
            registry = { token = "default-token" }

            [install.scopes]
            "@acme" = { token = "scope-token" }
            "#,
        );

        assert!(entries.contains(&(
            "@acme:registry".to_string(),
            "https://registry.npmjs.org/".to_string()
        )));
        assert!(entries.contains(&(
            "//registry.npmjs.org/:_authToken".to_string(),
            "default-token".to_string()
        )));
        assert!(
            !entries.iter().any(|(_, value)| value == "scope-token"),
            "same-registry scoped credentials are not representable as npmrc URI auth"
        );
    }
}
