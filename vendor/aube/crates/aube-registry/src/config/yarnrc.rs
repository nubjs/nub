use base64::Engine as _;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::url::{normalize_registry_url, registry_uri_key};

#[derive(Default)]
pub(super) struct SplitYarnrcEntries {
    pub user: Vec<(String, String)>,
    pub project: Vec<(String, String)>,
}

pub(super) fn load_yarnrc_entries_split(
    home: Option<&Path>,
    project_dir: &Path,
) -> SplitYarnrcEntries {
    if !aube_util::engine_context().read_yarn_config {
        return SplitYarnrcEntries::default();
    }
    load_yarnrc_entries_split_with_home(home, project_dir)
}

pub(super) fn load_yarnrc_entries_split_with_home(
    home: Option<&Path>,
    starting_dir: &Path,
) -> SplitYarnrcEntries {
    SplitYarnrcEntries {
        user: load_user_yarnrc_entries_with_home(home),
        project: load_project_yarnrc_entries_with_home(home, starting_dir),
    }
}

pub(super) fn load_user_yarnrc_entries(home: Option<&Path>) -> Vec<(String, String)> {
    if !aube_util::engine_context().read_yarn_config {
        return Vec::new();
    }
    load_user_yarnrc_entries_with_home(home)
}

fn load_user_yarnrc_entries_with_home(home: Option<&Path>) -> Vec<(String, String)> {
    let Some(home) = home else {
        return Vec::new();
    };
    let mut out = load_yarnrc_entries_from_path(&home.join(".yarnrc.yml"));
    // Classic Yarn (v1) reads `~/.yarnrc` in addition to the Berry
    // `.yarnrc.yml`. Core registry/auth fields only — see
    // `translate_classic_yarnrc_content`.
    out.extend(load_classic_yarnrc_entries_from_path(&home.join(".yarnrc")));
    out
}

pub(super) fn load_project_yarnrc_entries(starting_dir: &Path) -> Vec<(String, String)> {
    if !aube_util::engine_context().read_yarn_config {
        return Vec::new();
    }
    load_project_yarnrc_entries_with_home(aube_util::env::home_dir().as_deref(), starting_dir)
}

fn load_project_yarnrc_entries_with_home(
    home: Option<&Path>,
    starting_dir: &Path,
) -> Vec<(String, String)> {
    let per_file: Vec<Vec<(String, String)>> = yarnrc_paths_from_root(starting_dir, ".yarnrc.yml")
        .into_iter()
        .filter(|path| home.is_none_or(|home| path != &home.join(".yarnrc.yml")))
        .map(|path| load_yarnrc_entries_from_path(&path))
        .collect();
    let mut out = merge_project_yarnrc_entries(per_file);
    // Classic Yarn (v1) `.yarnrc` files along the same ancestor walk. Core
    // registry/auth fields only; appended after the Berry `.yarnrc.yml`
    // entries (root→child order so nearest still wins under the settings
    // layer's last-one-wins read).
    for path in yarnrc_paths_from_root(starting_dir, ".yarnrc")
        .into_iter()
        .filter(|path| home.is_none_or(|home| path != &home.join(".yarnrc")))
    {
        out.extend(load_classic_yarnrc_entries_from_path(&path));
    }
    out
}

/// Combine the per-file entry lists from the ancestor `.yarnrc.yml` walk
/// (root→child order) into the single concatenated list the settings layer
/// reads.
///
/// Scalar settings (registry/auth/nodeLinker) stay concatenated in order so the
/// settings reader's last-one-wins (`entries.iter().rev()`) keeps the nearest
/// file winning. `packageExtensions` is the exception: it is a map-typed
/// setting, and the settings reader is single-file last-wins, so emitting one
/// entry per file would silently drop every ancestor file's selectors. Yarn
/// instead shallow-merges map settings across all rc files
/// (`Object.assign({}, ...allFiles)` in `configUtils.resolveRcFiles`), so we
/// merge every file's `packageExtensions` object into ONE entry — root→child
/// order so a child file's value wins on a duplicate selector key, while
/// selectors unique to an ancestor file survive — and emit it once.
fn merge_project_yarnrc_entries(per_file: Vec<Vec<(String, String)>>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut merged_package_extensions: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut saw_package_extensions = false;

    for entries in per_file {
        for (key, value) in entries {
            if key == "packageExtensions" {
                if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str(&value) {
                    saw_package_extensions = true;
                    // Later (child) files win on a duplicate selector key; the
                    // BTreeMap insert overwrites, matching Yarn's shallow merge.
                    merged_package_extensions.extend(obj);
                }
            } else {
                out.push((key, value));
            }
        }
    }

    if saw_package_extensions
        && let Some(json) = package_extensions_json(&merged_package_extensions)
    {
        push(&mut out, "packageExtensions", json);
    }

    out
}

pub(super) fn yarn_env_entries_from(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut config = YarnRc::default();
    for (key, value) in env {
        match yarn_env_key(key).as_deref() {
            Some("npmRegistryServer") => config.npm_registry_server = Some(value.clone()),
            Some("npmAuthToken") => config.npm_auth_token = Some(value.clone()),
            Some("npmAuthIdent") => config.npm_auth_ident = Some(value.clone()),
            Some("nodeLinker") => config.node_linker = Some(value.clone()),
            // Yarn Berry resolves env vars with `camelcase(name)` to a flat
            // top-level setting key (`getEnvironmentSettings`), so only the
            // top-level network/TLS settings have a well-defined env spelling.
            // The map-shaped settings (`npmScopes`, `npmRegistries`,
            // `networkSettings`) are NOT reachable via env in Yarn itself —
            // there is no `YARN_<SCREAMING>` form that targets a map entry — so
            // there is nothing to translate for those.
            Some("httpsCaFilePath") => config.https_ca_file_path = Some(value.clone()),
            Some("httpProxy") => config.http_proxy = Some(value.clone()),
            Some("httpsProxy") => config.https_proxy = Some(value.clone()),
            Some("enableStrictSsl") => {
                config.enable_strict_ssl = aube_settings::parse_bool(value);
            }
            _ => {}
        }
    }
    config.into_entries()
}

pub(super) fn yarn_env_entries_from_std() -> Vec<(String, String)> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    yarn_env_entries_from(&env)
}

fn yarnrc_paths_from_root(starting_dir: &Path, filename: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = starting_dir.to_path_buf();
    loop {
        dirs.push(current.clone());
        if !current.pop() {
            break;
        }
    }
    dirs.reverse();
    dirs.into_iter()
        .map(|dir| dir.join(filename))
        .filter(|path| path.is_file())
        .collect()
}

fn load_yarnrc_entries_from_path(path: &Path) -> Vec<(String, String)> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    translate_yarnrc_content(&content)
}

pub(super) fn translate_yarnrc_content(content: &str) -> Vec<(String, String)> {
    let Ok(config) = aube_manifest::parse_yaml::<YarnRc>(Path::new(".yarnrc.yml"), content.into())
    else {
        return Vec::new();
    };
    config.into_entries()
}

fn load_classic_yarnrc_entries_from_path(path: &Path) -> Vec<(String, String)> {
    // Classic `.yarnrc` is read ONLY under a classic-Yarn (v1) incumbent. Yarn
    // Berry (v2+) abandoned `.yarnrc` for `.yarnrc.yml`, so a stray legacy
    // `.yarnrc` beside a Berry project is one Berry itself ignores — reading it
    // would silently diverge from Yarn (wrong registry/auth). The embedder sets
    // `yarn_is_classic` only when the active Yarn is provably v1; the Berry
    // `.yarnrc.yml` path above is gated by `read_yarn_config` alone and is
    // unaffected. Default (`false`) leaves standalone aube unchanged.
    if !aube_util::engine_context().yarn_is_classic {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    translate_classic_yarnrc_content(&content)
}

/// Translate a classic Yarn (v1) `.yarnrc` file to npmrc-shaped entries,
/// limited to the core registry/auth fields.
///
/// Classic `.yarnrc` uses the lockfile grammar's `key value` line form, where a
/// value is JSON-quoted when it contains characters that need escaping (so most
/// real-world registry/token lines are `key "value"`). Its config keys are
/// already npmrc-shaped — `registry`, `"@scope:registry"`,
/// `"//host/:_authToken"`, `"//host/:_auth"`, plus bare `_authToken` / `_auth`
/// — so translation is essentially identity over the supported subset: parse
/// each flat line, keep only the core keys, and normalize registry URLs to
/// match how the `.yarnrc.yml` path emits them.
///
/// Out of scope (and ignored): every other classic key (`network-timeout`,
/// `save-prefix`, `yarn-offline-mirror`, `--flag` arg lines, nested object
/// values, …). This is the same "basic core-field" level as the `.yarnrc.yml`
/// support, not full Yarn Classic parity.
pub(super) fn translate_classic_yarnrc_content(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let Some((key, value)) = parse_classic_yarnrc_line(line) else {
            continue;
        };
        if !classic_yarnrc_key_is_supported(&key) {
            continue;
        }
        if key == "registry" || key.ends_with(":registry") {
            push(&mut out, key, normalize_registry_url(&value));
        } else {
            push(&mut out, key, value);
        }
    }
    out
}

/// Core registry/auth keys we honor from a classic `.yarnrc`. Mirrors the
/// `.yarnrc.yml` subset: default registry, scoped registry, and registry-keyed
/// or top-level auth.
fn classic_yarnrc_key_is_supported(key: &str) -> bool {
    key == "registry"
        || key.ends_with(":registry")
        || key.ends_with(":_authToken")
        || key.ends_with(":_auth")
        || key == "_authToken"
        || key == "_auth"
}

/// Parse one flat `key value` line of a classic `.yarnrc`. Returns `None` for
/// blank lines, comments, indented (nested-object) lines, and `--flag` arg
/// lines. Both the key and the value may be JSON-double-quoted; bare tokens are
/// taken verbatim.
fn parse_classic_yarnrc_line(line: &str) -> Option<(String, String)> {
    // A leading space/tab marks an indented (nested-object) line in the
    // lockfile grammar — not a flat core-field line, so skip it.
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("--") {
        return None;
    }
    let (key_raw, rest) = split_classic_token(trimmed)?;
    let key = unquote_classic_token(key_raw);
    let value_raw = rest.trim();
    if value_raw.is_empty() {
        return None;
    }
    let (value_raw, _) = split_classic_token(value_raw)?;
    Some((key, unquote_classic_token(value_raw)))
}

/// Split off the first whitespace-delimited token, respecting a surrounding
/// pair of double quotes (so a quoted token containing spaces stays intact).
/// Returns the token slice and the remainder of the line.
fn split_classic_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    if let Some(after_open) = s.strip_prefix('"') {
        // Find the closing quote, honoring backslash escapes.
        let bytes = after_open.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => {
                    let end = 1 + i + 1; // include both quotes
                    let token = &s[..end];
                    let remainder = &s[end..];
                    return Some((token, remainder));
                }
                _ => i += 1,
            }
        }
        // Unterminated quote — treat the rest as the token.
        Some((s, ""))
    } else {
        match s.find(char::is_whitespace) {
            Some(idx) => Some((&s[..idx], &s[idx..])),
            None => Some((s, "")),
        }
    }
}

/// Strip a surrounding pair of double quotes and unescape, matching how the
/// classic lockfile parser JSON-decodes a quoted token. Bare tokens pass
/// through unchanged.
fn unquote_classic_token(token: &str) -> String {
    if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        serde_json::from_str::<String>(token).unwrap_or_else(|_| token.to_string())
    } else {
        token.to_string()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YarnRc {
    npm_registry_server: Option<String>,
    npm_auth_token: Option<String>,
    npm_auth_ident: Option<String>,
    // `npmAlwaysAuth: true` tells Yarn to attach the registry's
    // credentials to every request for that registry — including tarball
    // downloads on a different host than the registry, which are otherwise
    // sent unauthenticated. Translated to the npmrc-shaped `always-auth`
    // key (top-level here, `//host/:always-auth` per-registry, or scoped
    // via the owning scope's registry). See `into_entries`.
    npm_always_auth: Option<bool>,
    node_linker: Option<String>,
    // Top-level network/TLS settings. Yarn Berry expresses the CA bundle
    // as a *file path* (`httpsCaFilePath`) and the proxies as plain URLs
    // (`httpProxy` / `httpsProxy`), which map 1:1 onto the npmrc-shaped
    // `cafile` / `http-proxy` / `https-proxy` settings the registry client
    // already consumes. `enableStrictSsl` maps onto `strict-ssl`.
    https_ca_file_path: Option<String>,
    // mTLS client certificate / key. Yarn Berry expresses both as *file
    // paths* (`httpsCertFilePath` / `httpsKeyFilePath`). The registry
    // client's client-identity consumer takes inline PEM (`cert` / `key`),
    // not a path, so the file contents are loaded from disk at translate
    // time and emitted as the inline `cert` / `key` npmrc keys — see
    // `into_entries`.
    https_cert_file_path: Option<String>,
    https_key_file_path: Option<String>,
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    enable_strict_ssl: Option<bool>,
    #[serde(default)]
    npm_scopes: BTreeMap<String, YarnScope>,
    #[serde(default)]
    npm_registries: BTreeMap<String, YarnRegistry>,
    // Per-hostname network settings (`networkSettings.<host>.*`). Only the
    // CA bundle path is representable in nub's per-registry config model
    // (`//host/:cafile`); per-host proxy/TLS-key entries have no equivalent
    // (nub's proxy is process-wide) and glob host keys can't map onto the
    // exact-prefix auth model, so both are skipped — see `into_entries`.
    #[serde(default)]
    network_settings: BTreeMap<String, YarnNetworkSettings>,
    // Yarn Berry's `packageExtensions:` — a map of `pkg@range` selectors to
    // `{ dependencies, peerDependencies, peerDependenciesMeta }` shapes. The
    // value is captured verbatim (the YAML deserializer maps it straight into
    // `serde_json::Value`) and re-emitted as a JSON object string under the
    // `packageExtensions` settings key, so it flows through the exact same
    // object-setting merge + parser that pnpm's `pnpm.packageExtensions` does.
    // Yarn's shape mirrors the resolver's model 1:1 (Yarn omits
    // `optionalDependencies`, which the parser simply reads as empty). Captured
    // as a generic `serde_json::Value` rather than a typed struct so arbitrary
    // nested entries round-trip untouched.
    #[serde(default)]
    package_extensions: BTreeMap<String, serde_json::Value>,
    // Yarn Berry's `supportedArchitectures:` — `{ os, cpu, libc }` arrays,
    // each entry a concrete value or the literal `"current"` (the host
    // triple). The shape is identical to pnpm's
    // `pnpm.supportedArchitectures`, so it is captured verbatim and
    // re-emitted as a JSON object string under the `supportedArchitectures`
    // settings key — the same object-setting channel pnpm's value flows
    // through, where the install path unions it into the resolver's
    // platform filter. Captured as a generic `serde_json::Value` so the
    // arrays round-trip untouched.
    supported_architectures: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YarnScope {
    npm_registry_server: Option<String>,
    npm_auth_token: Option<String>,
    npm_auth_ident: Option<String>,
    npm_always_auth: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YarnRegistry {
    npm_auth_token: Option<String>,
    npm_auth_ident: Option<String>,
    npm_always_auth: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YarnNetworkSettings {
    https_ca_file_path: Option<String>,
    // Per-host mTLS client cert/key paths. Translated to the per-registry
    // inline `//host/:cert` / `//host/:key` keys (PEM loaded from disk),
    // mirroring how the top-level pair maps onto `cert` / `key`.
    https_cert_file_path: Option<String>,
    https_key_file_path: Option<String>,
}

impl YarnRc {
    fn into_entries(self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let default_registry = self
            .npm_registry_server
            .as_deref()
            .map(normalize_registry_url);
        let registry_configs = self
            .npm_registries
            .keys()
            .map(|registry| normalize_registry_url(registry))
            .collect::<BTreeSet<_>>();
        let scope_registry_counts = scope_registry_counts(&self.npm_scopes);

        if let Some(registry) = &default_registry {
            push(&mut out, "registry", registry.clone());
        }
        push_auth(
            &mut out,
            default_registry.as_deref(),
            self.npm_auth_token.as_deref(),
            self.npm_auth_ident.as_deref(),
        );
        // Top-level `npmAlwaysAuth` applies to the default registry. With
        // no default registry configured it scopes to the public registry
        // (npmrc's bare `always-auth` does the same), so emit the unscoped
        // key when there's no explicit default.
        if self.npm_always_auth == Some(true) {
            match &default_registry {
                Some(registry) => {
                    push(
                        &mut out,
                        format!("{}:always-auth", registry_uri_key(registry)),
                        "true",
                    );
                }
                None => push(&mut out, "always-auth", "true"),
            }
        }

        for (registry, config) in self.npm_registries {
            let registry = normalize_registry_url(&registry);
            push_auth(
                &mut out,
                Some(&registry),
                config.npm_auth_token.as_deref(),
                config.npm_auth_ident.as_deref(),
            );
            if config.npm_always_auth == Some(true) {
                push(
                    &mut out,
                    format!("{}:always-auth", registry_uri_key(&registry)),
                    "true",
                );
            }
        }

        for (scope, config) in self.npm_scopes {
            let scope = if scope.starts_with('@') {
                scope
            } else {
                format!("@{scope}")
            };
            let explicit_registry = config
                .npm_registry_server
                .as_deref()
                .map(normalize_registry_url);
            let registry = explicit_registry
                .clone()
                .or_else(|| default_registry.clone());
            if let Some(registry) = &registry {
                push(&mut out, format!("{scope}:registry"), registry.clone());
            }
            let scope_auth_is_representable = explicit_registry.as_ref().is_some_and(|registry| {
                Some(registry) != default_registry.as_ref()
                    && !registry_configs.contains(registry)
                    && scope_registry_counts.get(registry).copied().unwrap_or(0) == 1
            });
            // Yarn auth can be package-scope-specific. The existing registry
            // model cannot represent that, so only translate scope auth when
            // the scope owns a unique custom registry. Otherwise translating it
            // would widen the credential to every package fetched from the same
            // registry.
            if scope_auth_is_representable {
                push_auth(
                    &mut out,
                    registry.as_deref(),
                    config.npm_auth_token.as_deref(),
                    config.npm_auth_ident.as_deref(),
                );
                if config.npm_always_auth == Some(true)
                    && let Some(registry) = &registry
                {
                    push(
                        &mut out,
                        format!("{}:always-auth", registry_uri_key(registry)),
                        "true",
                    );
                }
            }
        }

        if let Some(linker) = self.node_linker.as_deref().map(str::trim) {
            match linker.to_ascii_lowercase().as_str() {
                "node-modules" => push(&mut out, "nodeLinker", "hoisted"),
                "pnpm" => push(&mut out, "nodeLinker", "isolated"),
                // PnP generation is out of scope. Leave it to nub's existing
                // Yarn-PnP warning/refusal path instead of pretending support.
                _ => {}
            }
        }

        // Top-level network/TLS settings → npmrc-shaped keys. These flow
        // through the same `apply_tagged` consumer the `.npmrc` path uses, so
        // they inherit the existing trust gate: a *project* `.yarnrc.yml`
        // setting a proxy / disabling strict-ssl is rejected with a warning
        // (untrusted source), while a user-level `~/.yarnrc.yml` is honored —
        // identical to how a project vs user `.npmrc` is treated.
        if let Some(cafile) = self.https_ca_file_path.as_deref() {
            push(&mut out, "cafile", cafile);
        }
        // mTLS client identity. The cert/key consumer takes inline PEM, so
        // read the referenced files here and emit `cert` / `key`. Both must
        // resolve for a usable identity; if either path is missing or
        // unreadable, neither is emitted (a half-identity is never valid).
        if let (Some(cert), Some(key)) = (
            read_pem(self.https_cert_file_path.as_deref()),
            read_pem(self.https_key_file_path.as_deref()),
        ) {
            push(&mut out, "cert", cert);
            push(&mut out, "key", key);
        }
        if let Some(proxy) = self.http_proxy.as_deref() {
            push(&mut out, "http-proxy", proxy);
        }
        if let Some(proxy) = self.https_proxy.as_deref() {
            push(&mut out, "https-proxy", proxy);
        }
        if let Some(strict) = self.enable_strict_ssl {
            // The npmrc consumer parses the value with `parse_bool`; emit the
            // canonical lowercase spelling.
            push(
                &mut out,
                "strict-ssl",
                if strict { "true" } else { "false" },
            );
        }

        // Per-host `networkSettings.<host>.httpsCaFilePath` → the per-registry
        // `//host/:cafile` form. Only literal (glob-free) host keys are
        // translated: nub's per-registry config is keyed by an exact `//host/`
        // prefix, so a Yarn glob pattern (`*.example.com`) has no faithful
        // mapping and is skipped rather than silently mis-scoped. Per-host
        // proxy / TLS-key/cert entries are likewise skipped (no representable
        // target). The host key becomes the URI-scoped `//<host>/:cafile`
        // entry the `.npmrc` consumer already understands.
        for (host, settings) in &self.network_settings {
            if host_is_glob(host) {
                continue;
            }
            let host_key = host.trim_matches('/');
            if let Some(cafile) = settings.https_ca_file_path.as_deref() {
                push(&mut out, format!("//{host_key}/:cafile"), cafile);
            }
            // Per-host mTLS identity → inline `//host/:cert` / `//host/:key`
            // (PEM loaded from disk). Both must resolve, same as the
            // top-level pair.
            if let (Some(cert), Some(key)) = (
                read_pem(settings.https_cert_file_path.as_deref()),
                read_pem(settings.https_key_file_path.as_deref()),
            ) {
                push(&mut out, format!("//{host_key}/:cert"), cert);
                push(&mut out, format!("//{host_key}/:key"), key);
            }
        }

        if !self.package_extensions.is_empty()
            && let Some(json) = package_extensions_json(&self.package_extensions)
        {
            push(&mut out, "packageExtensions", json);
        }

        // `supportedArchitectures` → the JSON-object `supportedArchitectures`
        // settings key, mirroring `packageExtensions`. The value must be a
        // JSON object (`{os,cpu,libc}`); anything else (a stray scalar) is
        // dropped rather than emitting a malformed entry the object-setting
        // reader would ignore anyway.
        if let Some(serde_json::Value::Object(obj)) = &self.supported_architectures
            && !obj.is_empty()
            && let Ok(json) = serde_json::to_string(&serde_json::Value::Object(obj.clone()))
        {
            push(&mut out, "supportedArchitectures", json);
        }

        out
    }
}

/// Read a PEM file referenced by a Yarn `httpsCertFilePath` /
/// `httpsKeyFilePath` setting and return its contents as the inline
/// `cert` / `key` value the registry client consumes. Returns `None`
/// when the path is absent, empty, or unreadable (a missing client
/// cert/key file is non-fatal — the install proceeds without an mTLS
/// identity rather than aborting, matching how the registry client
/// already tolerates an invalid cert/key pair). A warning is logged so
/// a misconfigured path is diagnosable.
fn read_pem(path: Option<&str>) -> Option<String> {
    let path = path.map(str::trim).filter(|p| !p.is_empty())?;
    match std::fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        Ok(_) => {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_CLIENT_CERT,
                "ignoring yarn mTLS cert/key path {path:?}: file is empty"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_CLIENT_CERT,
                "ignoring yarn mTLS cert/key path {path:?}: {e}"
            );
            None
        }
    }
}

/// True when a Yarn `networkSettings` host key contains a glob
/// metacharacter. nub's per-registry config matches on an exact `//host/`
/// prefix, so a glob pattern can't be translated without silently
/// widening or narrowing the entry — those are skipped instead.
fn host_is_glob(host: &str) -> bool {
    host.contains(['*', '?', '[', ']', '{', '}', '(', ')', '!', '+', '@'])
}

/// Serialize a parsed Yarn `packageExtensions:` map to a JSON object string
/// under the `packageExtensions` settings key. The settings layer reads that
/// key with `parse_json_object`, so the value must be a JSON *object* string.
/// Returns `None` if the map fails to round-trip to a JSON object (it never
/// should — every captured value is YAML-decodable — but we drop silently
/// rather than emit a malformed entry).
fn package_extensions_json(map: &BTreeMap<String, serde_json::Value>) -> Option<String> {
    let serde_json::Value::Object(obj) = serde_json::to_value(map).ok()? else {
        return None;
    };
    serde_json::to_string(&serde_json::Value::Object(obj)).ok()
}

fn push(out: &mut Vec<(String, String)>, key: impl Into<String>, value: impl Into<String>) {
    let value = value.into();
    if !value.trim().is_empty() {
        out.push((key.into(), value));
    }
}

fn push_auth(
    out: &mut Vec<(String, String)>,
    registry: Option<&str>,
    token: Option<&str>,
    ident: Option<&str>,
) {
    let Some(registry) = registry else {
        return;
    };
    let uri = registry_uri_key(registry);
    if let Some(token) = token.filter(|v| !v.trim().is_empty()) {
        push(out, format!("{uri}:_authToken"), token);
    }
    if let Some(ident) = ident.filter(|v| !v.trim().is_empty()) {
        push(
            out,
            format!("{uri}:_auth"),
            yarn_auth_ident_to_npm_auth(ident),
        );
    }
}

fn yarn_auth_ident_to_npm_auth(ident: &str) -> String {
    if ident.contains(':') {
        base64::engine::general_purpose::STANDARD.encode(ident)
    } else {
        ident.to_string()
    }
}

fn scope_registry_counts(scopes: &BTreeMap<String, YarnScope>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for scope in scopes.values() {
        if let Some(registry) = scope
            .npm_registry_server
            .as_deref()
            .map(normalize_registry_url)
        {
            *counts.entry(registry).or_insert(0) += 1;
        }
    }
    counts
}

fn yarn_env_key(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let rest = lower.strip_prefix("yarn_")?;
    match rest {
        "npm_registry_server" => Some("npmRegistryServer".to_string()),
        "npm_auth_token" => Some("npmAuthToken".to_string()),
        "npm_auth_ident" => Some("npmAuthIdent".to_string()),
        "node_linker" => Some("nodeLinker".to_string()),
        "https_ca_file_path" => Some("httpsCaFilePath".to_string()),
        "http_proxy" => Some("httpProxy".to_string()),
        "https_proxy" => Some("httpsProxy".to_string()),
        "enable_strict_ssl" => Some("enableStrictSsl".to_string()),
        _ => None,
    }
}
