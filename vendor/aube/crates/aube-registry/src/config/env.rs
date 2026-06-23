/// Synthesize `.npmrc`-style entries from a captured `npm_config_*` /
/// `NPM_CONFIG_*` environment-variable slice so [`NpmConfig::apply`]
/// can consume them uniformly. Only registry-client-owned keys (the
/// default registry, scoped registries, per-URI auth, proxies, TLS
/// knobs) are emitted — generic pnpm settings are already surfaced
/// via `aube_settings::resolved::*`, which consults its own env-var
/// aliases. Env entries must be applied *after* `.npmrc` entries so
/// last-write-wins gives env the higher precedence npm/pnpm document.
pub(super) fn npm_config_env_entries_from(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut npm_scoped = Vec::new();
    let mut pnpm_scoped = Vec::new();
    let mut out = Vec::new();
    for (name, value) in env {
        if value.is_empty() {
            continue;
        }
        match translate_npm_config_env(name, value) {
            Some((key, value)) if key.starts_with("//") => {
                if name
                    .get(.."pnpm_config_".len())
                    .is_some_and(|p| p.eq_ignore_ascii_case("pnpm_config_"))
                {
                    pnpm_scoped.push((key, value));
                } else {
                    npm_scoped.push((key, value));
                }
            }
            Some(entry) => out.push(entry),
            None => {}
        }
    }
    out.extend(npm_scoped);
    out.extend(pnpm_scoped);
    out
}

/// Map a single `npm_config_*` / `NPM_CONFIG_*` env var to the
/// `.npmrc`-style `(key, value)` that [`NpmConfig::apply`] understands.
/// Returns `None` for env vars unrelated to registry-client config —
/// those are owned by the generic settings resolver. Pure function so
/// tests can exercise the mapping without mutating `std::env`.
pub(super) fn translate_npm_config_env(name: &str, value: &str) -> Option<(String, String)> {
    let suffix = name
        .strip_prefix("npm_config_")
        .or_else(|| name.strip_prefix("NPM_CONFIG_"))
        .or_else(|| strip_url_scoped_config_prefix(name))?;
    // Per-URI auth keys (e.g. `//registry.example.com/:_authToken`)
    // already carry `.npmrc` syntax in the env-var name. Pass them
    // through unchanged so `apply`'s `starts_with("//")` arm picks
    // them up and preserves the `_authToken` / `_auth` / `username`
    // casing that the match inside it depends on.
    if suffix.starts_with("//") && is_url_scoped_env_auth_key(suffix) {
        return Some((suffix.to_string(), value.to_string()));
    }
    // Scoped-registry keys: `@myorg:REGISTRY` or `@MYORG:registry`,
    // translated to the canonical `@myorg:registry` form. The scope
    // segment is lowercased because npm scope names are
    // case-insensitive on the registry side, and `apply` matches the
    // `:registry` suffix literally.
    if let Some(rest) = suffix.strip_prefix('@')
        && let Some((scope, tail)) = rest.split_once(':')
        && tail.eq_ignore_ascii_case("registry")
    {
        return Some((
            format!("@{}:registry", scope.to_ascii_lowercase()),
            value.to_string(),
        ));
    }
    // Canonical single-word or `_`-separated multi-word keys. The
    // left column is the lowercased env-suffix (POSIX-style); the
    // right column is the `.npmrc` key `apply` matches on.
    let npmrc_key = match suffix.to_ascii_lowercase().as_str() {
        "registry" => "registry",
        "https_proxy" => "https-proxy",
        "http_proxy" => "http-proxy",
        "proxy" => "proxy",
        "noproxy" => "noproxy",
        "strict_ssl" => "strict-ssl",
        "local_address" => "local-address",
        "maxsockets" => "maxsockets",
        _ => return None,
    };
    Some((npmrc_key.to_string(), value.to_string()))
}

/// Synthesize `.npmrc`-style entries from Bun's `BUN_CONFIG_REGISTRY` /
/// `BUN_CONFIG_TOKEN` install-registry environment variables so
/// [`NpmConfig::apply_tagged`] can consume them uniformly. Only emitted when
/// the embedder has set [`EngineContext::read_bun_config`] (Bun is the active
/// incumbent); standalone aube never reads these.
///
/// Mirrors Bun's `PackageManagerOptions` env handling
/// (`src/install/PackageManager/PackageManagerOptions.zig`):
///
/// - `BUN_CONFIG_REGISTRY` → the default `registry`, but *only* when it parses
///   as an `http://` / `https://` URL — Bun ignores any other value. This is
///   the highest-precedence default-registry source (checked before
///   `NPM_CONFIG_REGISTRY` / `npm_config_registry`), so the caller appends
///   these entries *after* the `npm_config_*` entries for last-write-wins.
/// - `BUN_CONFIG_TOKEN` → the default registry's `_authToken`. Emitted as an
///   unscoped `_authToken` tagged [`NpmrcSource::Env`]; `apply_tagged` pins it
///   to the env source's resolved default registry (the `BUN_CONFIG_REGISTRY`
///   URL when set, else `registry.npmjs.org`). A `BUN_CONFIG_TOKEN` set without
///   `BUN_CONFIG_REGISTRY` against a *file*-configured custom default registry
///   therefore pins to npmjs.org rather than the file registry — the same
///   source-slot limitation the `npm_config`/yarn env tokens have, and a rare
///   case versus the common CI pattern of setting both together.
pub(super) fn bun_env_entries_from(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(registry) = bun_env_get(env, "BUN_CONFIG_REGISTRY")
        && (registry.starts_with("https://") || registry.starts_with("http://"))
    {
        out.push(("registry".to_string(), registry.to_string()));
    }
    if let Some(token) = bun_env_get(env, "BUN_CONFIG_TOKEN") {
        out.push(("_authToken".to_string(), token.to_string()));
    }
    out
}

/// Capture-slice equivalent of `std::env::var` for the Bun env keys. Returns
/// the first non-empty value, matching Bun's `env.get(key)` + `len > 0` gate.
fn bun_env_get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// `std::env`-reading wrapper over [`bun_env_entries_from`], used on the
/// non-injected load paths (`load_*_split`, the scoped readers).
pub(super) fn bun_env_entries_from_std() -> Vec<(String, String)> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    bun_env_entries_from(&env)
}

fn strip_url_scoped_config_prefix(name: &str) -> Option<&str> {
    for prefix in ["npm_config_", "pnpm_config_"] {
        if name
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        {
            let suffix = &name[prefix.len()..];
            if suffix.starts_with("//") {
                return Some(suffix);
            }
        }
    }
    None
}

fn is_url_scoped_env_auth_key(key: &str) -> bool {
    key.rsplit_once(':').is_some_and(|(_, suffix)| {
        matches!(suffix, "_authToken" | "_auth" | "username" | "_password")
    })
}
/// Return the first set (and non-empty) env var in `names`. Used to
/// read proxy config from both the upper- and lowercase spellings that
/// curl / node conventionally accept.
pub(super) fn env_any(names: &[&str]) -> Option<String> {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                // Trim before returning so a shell-quoted value like
                // `HTTPS_PROXY=" http://proxy "` doesn't slip past
                // `reqwest::Proxy::https` with surrounding whitespace
                // and silently fail.
                return Some(trimmed.to_string());
            }
        }
    }
    None
}
