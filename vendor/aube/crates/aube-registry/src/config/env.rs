/// Synthesize `.npmrc`-style entries from a captured `npm_config_*` /
/// `NPM_CONFIG_*` environment-variable slice so [`NpmConfig::apply`]
/// can consume them uniformly. Only registry-client-owned keys (the
/// default registry, scoped registries, per-URI auth, proxies, TLS
/// knobs) are emitted — generic pnpm settings are already surfaced
/// via `aube_settings::resolved::*`, which consults its own env-var
/// aliases. Env entries must be applied *after* `.npmrc` entries so
/// last-write-wins gives env the higher precedence npm/pnpm document.
///
/// Reads [`EngineContext::read_pnpm_config_env_registry`] to decide whether
/// bare `pnpm_config_<key>` / `PNPM_CONFIG_<KEY>` registry-client keys are
/// honored too (pnpm v11+ incumbent only); off by default, so standalone
/// aube and non-pnpm-v11 paths emit exactly the `npm_config_*` set as before.
///
/// [`EngineContext::read_pnpm_config_env_registry`]: aube_util::EngineContext::read_pnpm_config_env_registry
pub(super) fn npm_config_env_entries_from(env: &[(String, String)]) -> Vec<(String, String)> {
    let read_pnpm = aube_util::engine_context().read_pnpm_config_env_registry;
    let mut npm_scoped = Vec::new();
    let mut pnpm_scoped = Vec::new();
    let mut out = Vec::new();
    let mut pnpm_out = Vec::new();
    for (name, value) in env {
        if value.is_empty() {
            continue;
        }
        // A `pnpm_config_`-prefixed source. Under the gate, real pnpm 11
        // reads ONLY `pnpm_config_*` and ignores `npm_config_*`, so when both
        // spell the same key the pnpm one must win deterministically — env
        // iteration order is arbitrary, so a single `out` bucket would pick a
        // nondeterministic winner. Route pnpm-prefixed entries to their own
        // bucket appended LAST (last-write-wins → pnpm beats its npm twin),
        // mirroring the `//`-auth two-bucket ordering below.
        let is_pnpm_prefixed = name
            .get(.."pnpm_config_".len())
            .is_some_and(|p| p.eq_ignore_ascii_case("pnpm_config_"));
        match translate_config_env(name, value, read_pnpm) {
            Some((key, value)) if key.starts_with("//") => {
                if is_pnpm_prefixed {
                    pnpm_scoped.push((key, value));
                } else {
                    npm_scoped.push((key, value));
                }
            }
            Some(entry) if is_pnpm_prefixed => pnpm_out.push(entry),
            Some(entry) => out.push(entry),
            None => {}
        }
    }
    // Order (low → high precedence, last-write-wins): npm non-auth, pnpm
    // non-auth, npm auth, pnpm auth. The auth buckets stay after the non-auth
    // ones to preserve the existing auth-wins layering.
    out.extend(pnpm_out);
    out.extend(npm_scoped);
    out.extend(pnpm_scoped);
    out
}

/// Map a single registry-client env var to the `.npmrc`-style `(key, value)`
/// that [`NpmConfig::apply`] understands. Accepts the `npm_config_*` /
/// `NPM_CONFIG_*` family unconditionally and the `//`-scoped-auth
/// `pnpm_config_*` carve-out (upstream behavior). When `read_pnpm` is `true`
/// (pnpm v11+ incumbent), it ALSO accepts bare `pnpm_config_<key>` /
/// `PNPM_CONFIG_<KEY>` registry-client keys, mirroring pnpm 11's env reader
/// (`config/reader/src/env.ts`): the suffix must be strictly snake_case in a
/// single case — all-lowercase under `pnpm_config_`, all-uppercase under
/// `PNPM_CONFIG_` — which is why neither the `//`-auth nor the
/// `@scope:registry` form is reachable through this prefix on pnpm 11.
///
/// Returns `None` for env vars unrelated to registry-client config — those are
/// owned by the generic settings resolver. Pure function (the embedder gate is
/// passed in) so tests can exercise the mapping without mutating `std::env` or
/// the engine context.
pub(super) fn translate_config_env(
    name: &str,
    value: &str,
    read_pnpm: bool,
) -> Option<(String, String)> {
    let suffix = name
        .strip_prefix("npm_config_")
        .or_else(|| name.strip_prefix("NPM_CONFIG_"))
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| strip_url_scoped_config_prefix(name).map(std::borrow::Cow::Borrowed))
        // pnpm v11+ only: bare `pnpm_config_<snake>` / `PNPM_CONFIG_<SNAKE>`.
        // Returns the suffix already lowercased (pnpm camel-cases internally;
        // the `.npmrc`-key match below lowercases too, so a lowercased suffix
        // feeds it cleanly). The strict-snake gate keeps `//`-auth and
        // `@scope:registry` forms out of this branch — exactly as pnpm 11
        // rejects them — so they fall through to the existing
        // `strip_url_scoped_config_prefix` carve-out (or to `None`).
        .or_else(|| {
            read_pnpm
                .then(|| strip_bare_pnpm_config_prefix(name))
                .flatten()
                .map(std::borrow::Cow::Owned)
        })?;
    let suffix = suffix.as_ref();
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
        // Both the collapsed `noproxy` and underscored `no_proxy` spellings:
        // pnpm 11 kebab-cases the suffix, so `no_proxy` → `no-proxy` (a key
        // in npm's config schema) is honored alongside the bare `noproxy`.
        "noproxy" | "no_proxy" => "noproxy",
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

/// Strip a bare `pnpm_config_<suffix>` / `PNPM_CONFIG_<SUFFIX>` prefix,
/// returning the suffix LOWERCASED, but only when the suffix is strictly
/// snake_case in a SINGLE case — all-lowercase under the lowercase prefix,
/// all-uppercase under the uppercase prefix. Mirrors pnpm 11's
/// `getEnvKeySuffix` (`config/reader/src/env.ts`): pnpm accepts only those two
/// forms (shell convention favors the uppercase spelling, and the v11
/// migration guide renames `NPM_CONFIG_*` → `PNPM_CONFIG_*`), and rejects a
/// mixed-case or non-snake suffix outright. The strict-snake gate is also what
/// keeps the `//`-auth and `@scope:registry` shapes — which carry `/`, `:`,
/// `@` — out of this branch, matching pnpm 11 (those forms are unreadable via
/// `pnpm_config_*` there).
fn strip_bare_pnpm_config_prefix(name: &str) -> Option<String> {
    if let Some(suffix) = name.strip_prefix("pnpm_config_")
        && is_snake_case(suffix, char::is_ascii_lowercase)
    {
        return Some(suffix.to_string());
    }
    if let Some(suffix) = name.strip_prefix("PNPM_CONFIG_")
        && is_snake_case(suffix, char::is_ascii_uppercase)
    {
        return Some(suffix.to_ascii_lowercase());
    }
    None
}

/// A non-empty string whose `_`-separated segments are each non-empty and
/// composed solely of ASCII digits plus letters accepted by `letter_ok`.
/// Mirrors pnpm's `isLowerSnakeCase` / `isUpperSnakeCase`
/// (`/^[a-z0-9]+$/` / `/^[A-Z0-9]+$/` per segment).
fn is_snake_case(s: &str, letter_ok: impl Fn(&char) -> bool) -> bool {
    !s.is_empty()
        && s.split('_')
            .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit() || letter_ok(&c)))
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
