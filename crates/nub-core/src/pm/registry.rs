//! Registry metadata lookup for PM provisioning: dynamic version resolution
//! (exact / dist-tag / range), the tarball URL, and the dist integrity that gates
//! extraction. Mirrors `version_management::node_index`'s split — a PURE resolver
//! over already-fetched metadata ([`resolve_dist`]) plus a thin networked wrapper
//! ([`resolve_version`]) — so the resolution logic is unit-tested offline.
//!
//! Trust model: HTTPS authenticates that the packument came from the registry;
//! the per-version `dist.integrity` (sha512) authenticates the tarball before it
//! is extracted. No signatures / Sigstore / TUF in scope. sha512 is preferred;
//! `dist.shasum` (sha1) is the fail-closed fallback for ancient publishes that
//! predate `integrity`.

use std::path::{Component, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha512};

use crate::version_management::download::{self, Auth};
use crate::workspace::scripts::npmrc_value;

/// A single resolved version's dist: where to fetch it, how to verify it, and the
/// path within the extracted `package/` dir to the runnable bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDist {
    pub version: String,
    pub tarball: String,
    pub integrity: Integrity,
    /// The bin entry's path relative to the package root (`bin/pnpm.cjs`). For a
    /// PM the resolver picks the entry whose name matches the package.
    pub bin_subpath: PathBuf,
}

/// The dist checksum that gates extraction. sha512 (the modern `dist.integrity`
/// SRI hash, base64) is preferred; sha1 (`dist.shasum`, hex) is the fallback for
/// publishes too old to carry `integrity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// SRI sha512 payload, base64 (the part after `sha512-`).
    Sha512(String),
    /// `dist.shasum`, lowercase hex.
    Sha1(String),
}

/// The public npm registry — the floor of the precedence stack and the marker
/// for "no mirror configured" (the tarball-origin rewrite is a no-op against it).
pub const PUBLIC_REGISTRY: &str = "https://registry.npmjs.org";

/// The resolved registry for PM downloads: its base URL plus any auth that
/// applies to the base's host. Carries enough to fetch the packument AND the
/// tarball — both must present the same `Authorization` to an auth-required
/// mirror, and the tarball URL is rewritten onto `base`'s origin (see
/// [`rewrite_tarball_origin`]) when `base` is non-public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryConfig {
    /// Trailing-slash-trimmed base URL — callers concatenate `/<pkg>`.
    pub base: String,
    /// The `Authorization` credential for `base`'s host, if any.
    pub auth: Option<Auth>,
}

/// The registry base URL, in precedence order:
///   1. `npm_config_registry` (npm/pnpm/yarn all export this when they shell out).
///   2. `.npmrc`'s `registry` key (project, then `~/.npmrc`).
///   3. the public registry.
///
/// Thin wrapper over [`registry_config`] — the auth-free base for the callers
/// (and tests) that only need the URL. `COREPACK_NPM_REGISTRY` is the most-specific
/// override on top of this stack; see [`registry_config`].
pub fn registry_base(root: &std::path::Path) -> String {
    registry_config(root).base
}

/// The full registry config — base + host auth — for PM downloads, in precedence
/// order (most specific first):
///   1. `COREPACK_NPM_REGISTRY` (+ `COREPACK_NPM_TOKEN` / `_USERNAME`+`_PASSWORD`):
///      the only convention for a PM-download registry distinct from the dep
///      registry. When set, its companion auth vars are the ONLY auth consulted
///      (a deliberate clean override — you don't blend a corepack registry with
///      `.npmrc` host auth).
///   2. `npm_config_registry` (exported by npm/pnpm/yarn when they shell out).
///   3. `.npmrc`'s `registry` key (project, then `~/.npmrc`).
///   4. the public registry.
///
/// For sources 2–4, auth comes from `.npmrc` `//host[/path]/:_authToken` (bearer)
/// or `:_auth` / `:username`+`:_password` (basic), longest-host-prefix match
/// against the resolved base — npm's own resolution. `${VAR}` interpolation is
/// honored throughout (npm expands env in `.npmrc` values). Behavioral `COREPACK_*`
/// vars (STRICT/AUTO_PIN/HOME/…) are NOT consulted — they map to nub's own surface.
pub fn registry_config(root: &std::path::Path) -> RegistryConfig {
    // 1. COREPACK_NPM_REGISTRY wins outright, with its own companion auth.
    if let Some(raw) = env_nonempty("COREPACK_NPM_REGISTRY") {
        let base = interpolate_env(&raw).trim_end_matches('/').to_string();
        return RegistryConfig {
            base,
            auth: corepack_auth(),
        };
    }

    // 2–4. The ecosystem-standard stack: env override, then `.npmrc registry`,
    // then public. The selection rule is the pure [`resolve_base`].
    let base = resolve_base(
        env_nonempty("npm_config_registry"),
        npmrc_value(root, "registry"),
    );
    let auth = npmrc_auth_for(root, &base);
    RegistryConfig { base, auth }
}

/// PURE base selection for the ecosystem-standard stack (the COREPACK override is
/// handled by the caller, ABOVE this): `npm_config_registry` wins over the
/// `.npmrc registry` value, which wins over the public registry. Trailing slash
/// trimmed; `${VAR}` interpolated. Unit-tested without mutating process env.
fn resolve_base(npm_config_registry: Option<String>, npmrc_registry: Option<String>) -> String {
    let raw = npm_config_registry
        .filter(|s| !s.trim().is_empty())
        .or(npmrc_registry)
        .unwrap_or_else(|| PUBLIC_REGISTRY.to_string());
    interpolate_env(&raw).trim_end_matches('/').to_string()
}

/// `COREPACK_NPM_TOKEN` (bearer) wins over `COREPACK_NPM_USERNAME`+`_PASSWORD`
/// (basic). Username/password are base64-encoded into a Basic credential (npm's
/// `_auth` form). `${VAR}` interpolation applies to each.
fn corepack_auth() -> Option<Auth> {
    if let Some(tok) = env_nonempty("COREPACK_NPM_TOKEN") {
        return Some(Auth::Bearer(interpolate_env(&tok)));
    }
    let user = env_nonempty("COREPACK_NPM_USERNAME")?;
    let pass = env_nonempty("COREPACK_NPM_PASSWORD").unwrap_or_default();
    Some(Auth::Basic(base64_encode(
        format!("{}:{}", interpolate_env(&user), interpolate_env(&pass)).as_bytes(),
    )))
}

/// Read `VAR` from the environment, treating empty/whitespace as unset.
fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.trim().is_empty())
}

/// Expand `${VAR}` references against the process environment — the wrapper over
/// the pure [`interpolate_with`], with `std::env::var` as the lookup.
fn interpolate_env(value: &str) -> String {
    interpolate_with(value, |name| std::env::var(name).ok())
}

/// Expand `${VAR}` references in an `.npmrc` / env value, the way npm does, using
/// `lookup` to resolve each name (PURE — the env source is injected so the
/// expansion rules are unit-tested without mutating the process environment). An
/// undefined variable expands to the empty string (npm's behavior); `$VAR`
/// without braces is left verbatim (npm only interpolates the braced form). The
/// common shapes are `${NPM_TOKEN}` in a token line and `${HOME}` in a path.
fn interpolate_with(value: &str, mut lookup: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = value[i + 2..].find('}') {
                let name = &value[i + 2..i + 2 + close];
                out.push_str(&lookup(name).unwrap_or_default());
                i = i + 2 + close + 1;
                continue;
            }
        }
        let ch = value[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Collect every line of the relevant `.npmrc` files (project, then `~/.npmrc`)
/// and pick the auth credential whose `//host[/path]/` prefix is the longest one
/// matching `registry_base`. This is npm's nerfDart resolution: an auth line is
/// keyed by a registry URL minus its scheme (`//npm.example.com/path/:_authToken`),
/// and the most-specific (longest) matching prefix wins.
fn npmrc_auth_for(root: &std::path::Path, registry_base: &str) -> Option<Auth> {
    let mut text = String::new();
    let candidates = [
        root.join(".npmrc"),
        dirs_next::home_dir()
            .map(|h| h.join(".npmrc"))
            .unwrap_or_default(),
    ];
    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            text.push_str(&content);
            text.push('\n');
        }
    }
    parse_npmrc_auth(&text, registry_base, |name| std::env::var(name).ok())
}

/// PURE auth resolver over already-read `.npmrc` text — no filesystem, no network,
/// env injected via `lookup` — so longest-prefix / token-vs-basic /
/// env-interpolation are unit-tested offline. `registry_base` is the resolved
/// registry URL (e.g. `https://npm.example.com/artifactory/api/npm/npm`); the
/// chosen credential is the one whose `//host[/path]` key is the longest prefix of
/// the base (compared scheme-stripped, npm's nerfDart form).
///
/// Per host-prefix, `:_authToken` (bearer) wins over `:_auth` (basic) wins over
/// `:username`+`:_password` (basic). All values are `${VAR}`-interpolated through
/// `lookup`.
fn parse_npmrc_auth(
    npmrc: &str,
    registry_base: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Option<Auth> {
    // The base, scheme-stripped and trailing-slash-trimmed: `//host/path`.
    let base_nerf = strip_scheme(registry_base).trim_end_matches('/');

    // Group auth fields by their `//host[/path]` prefix.
    #[derive(Default)]
    struct Fields {
        auth_token: Option<String>,
        auth_basic: Option<String>,
        username: Option<String>,
        password: Option<String>,
    }
    let mut by_prefix: rustc_hash::FxHashMap<String, Fields> = rustc_hash::FxHashMap::default();

    for line in npmrc.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, raw_val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // Only `//…:field` lines carry auth. The prefix is everything before the
        // final `:` that introduces the field name.
        if !key.starts_with("//") {
            continue;
        }
        let Some((prefix, field)) = key.rsplit_once(':') else {
            continue;
        };
        let prefix = prefix.trim_end_matches('/');
        let val = interpolate_with(
            raw_val.trim().trim_matches('"').trim_matches('\''),
            &mut lookup,
        );
        let entry = by_prefix.entry(prefix.to_string()).or_default();
        match field {
            "_authToken" => entry.auth_token = Some(val),
            "_auth" => entry.auth_basic = Some(val),
            "username" => entry.username = Some(val),
            "_password" => entry.password = Some(decode_npmrc_password(&val)),
            _ => {}
        }
    }

    // Longest `//host[/path]` prefix that is a prefix of the base wins.
    let best = by_prefix
        .keys()
        .filter(|prefix| {
            let p = prefix.trim_start_matches("//");
            let base = base_nerf.trim_start_matches("//");
            base == p || base.starts_with(&format!("{p}/"))
        })
        .max_by_key(|prefix| prefix.len())
        .cloned()?;
    let f = by_prefix.get(&best)?;

    if let Some(tok) = f.auth_token.as_ref().filter(|t| !t.is_empty()) {
        return Some(Auth::Bearer(tok.clone()));
    }
    if let Some(basic) = f.auth_basic.as_ref().filter(|b| !b.is_empty()) {
        return Some(Auth::Basic(basic.clone()));
    }
    if let Some(user) = f.username.as_ref().filter(|u| !u.is_empty()) {
        let pass = f.password.clone().unwrap_or_default();
        return Some(Auth::Basic(base64_encode(
            format!("{user}:{pass}").as_bytes(),
        )));
    }
    None
}

/// npm stores `:_password` base64-encoded; decode it back to plaintext before
/// re-encoding `user:pass`. A value that doesn't decode (someone wrote a literal)
/// is used verbatim — fail-soft, since a malformed password line shouldn't abort
/// provisioning before the registry even gets a chance to reject it.
fn decode_npmrc_password(b64: &str) -> String {
    match base64_decode(b64) {
        Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| b64.to_string()),
        Err(_) => b64.to_string(),
    }
}

/// Strip the `https:` / `http:` scheme, leaving the `//host/path` nerfDart form
/// npm keys auth lines by. A value with no scheme is returned unchanged.
fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("https:")
        .or_else(|| url.strip_prefix("http:"))
        .unwrap_or(url)
}

/// Rewrite a packument's `dist.tarball` so it is fetched from the SAME registry
/// the packument came from. `dist.tarball` is an ABSOLUTE URL that the publisher
/// (or a replicating mirror) often hardcodes to the public registry even when the
/// packument was served by a private mirror — so a mirrored/air-gapped install
/// would otherwise download the tarball from the wrong (often unreachable) host.
/// npm/pnpm rewrite the origin: keep the path + query, swap scheme+host+port to
/// the configured registry's. Only rewritten when a NON-public registry is
/// configured; a public-registry config leaves the URL untouched (the common
/// case, and the safe one — never redirect a public tarball).
pub fn rewrite_tarball_origin(tarball: &str, registry_base: &str) -> String {
    // No rewrite when the configured registry is the public one.
    if origin_of(registry_base) == Some(origin_of(PUBLIC_REGISTRY).unwrap()) {
        return tarball.to_string();
    }
    let (Some(reg_origin), Some(tar_origin)) = (origin_of(registry_base), origin_of(tarball))
    else {
        return tarball.to_string(); // unparseable → leave it alone
    };
    if reg_origin == tar_origin {
        return tarball.to_string(); // already on the mirror
    }
    // Swap the origin (scheme+host[+port]); keep the rest of the URL verbatim.
    let rest = &tarball[tar_origin.len()..];
    format!("{reg_origin}{rest}")
}

/// The registry credential to present when DOWNLOADING `tarball_url` — `cfg.auth`,
/// but ONLY when the tarball is served from the SAME `scheme://host[:port]` origin
/// the credential belongs to. `dist.tarball` is an absolute URL the packument
/// declares, so under a malicious / MITM'd registry it can name an ARBITRARY
/// foreign host; attaching the registry bearer token (`_authToken`) to that
/// request would disclose the credential in flight (N1b). The packument fetch is
/// unaffected — it always targets `cfg.base` and keeps full auth; this gate is
/// only for the tarball download. For a private mirror [`rewrite_tarball_origin`]
/// has already pinned the tarball onto the registry's own origin, so this matches
/// and auth is attached exactly as before; the public-registry tarball is on the
/// registry host, so it matches too. The only request that loses auth is one to a
/// host the credential was never issued for — the leak. Mirrors aube matching
/// tarball auth to the tarball's own host.
pub fn auth_for_tarball<'a>(cfg: &'a RegistryConfig, tarball_url: &str) -> Option<&'a Auth> {
    let reg_origin = origin_of(&cfg.base)?;
    let tar_origin = origin_of(tarball_url)?;
    // scheme + host are ASCII-case-insensitive; the origin carries no path/query.
    if reg_origin.eq_ignore_ascii_case(tar_origin) {
        cfg.auth.as_ref()
    } else {
        None
    }
}

/// The `scheme://host[:port]` origin of a URL — everything up to (not including)
/// the first `/` after the `://`. Returns `None` for a URL with no `://`.
fn origin_of(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")?;
    let after = scheme_end + 3;
    let host_len = url[after..].find('/').unwrap_or(url.len() - after);
    Some(&url[..after + host_len])
}

/// Resolve `spec` against an already-fetched packument. PURE — no network, no env
/// — so the resolution rules are unit-tested offline. Handles:
///   - an exact `X.Y.Z` (verbatim key lookup; a typo'd version errors here rather
///     than attempting a doomed download),
///   - a dist-tag (`latest`, `next`, …) via the `dist-tags` map,
///   - a semver range (`^9`, `10`, `>=9 <11`) via the highest satisfying key.
///
/// Range parsing goes through Cargo's `semver` crate, which requires comparators
/// be comma-separated; npm/node-semver (what `packageManager` / `devEngines` users
/// write) separates them by space. [`normalize_range`] bridges the two so a
/// `>=9 <11` pin resolves rather than erroring. The `||` OR operator is NOT
/// supported (Cargo's `semver` has no OR) — vanishingly rare in a PM pin.
pub fn resolve_dist(packument: &Value, spec: &str) -> Result<VersionDist> {
    let spec = spec.trim();
    let versions = packument
        .get("versions")
        .and_then(Value::as_object)
        .context("packument has no \"versions\" map")?;

    // 1. A dist-tag short-circuits to its concrete version.
    if let Some(tagged) = packument
        .get("dist-tags")
        .and_then(|t| t.get(spec))
        .and_then(Value::as_str)
    {
        let meta = versions.get(tagged).with_context(|| {
            format!("dist-tag \"{spec}\" points at {tagged}, absent from \"versions\"")
        })?;
        return dist_from_meta(tagged, meta);
    }

    // 2. An exact version is a verbatim key lookup.
    if let Some(meta) = versions.get(spec) {
        return dist_from_meta(spec, meta);
    }

    // 3. Otherwise treat the spec as a semver range and pick the highest match.
    let req = semver::VersionReq::parse(&normalize_range(spec)).with_context(|| {
        format!("\"{spec}\" is not an exact version, dist-tag, or semver range")
    })?;
    let best = versions
        .keys()
        .filter_map(|v| semver::Version::parse(v).ok().map(|parsed| (parsed, v)))
        .filter(|(parsed, _)| req.matches(parsed))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, key)| key)
        .with_context(|| format!("no published version satisfies \"{spec}\""))?;
    dist_from_meta(best, &versions[best])
}

/// Translate a node-semver range into the form Cargo's `semver` crate parses:
/// space-separated comparators (`>=9 <11`) become comma-separated (`>=9, <11`).
/// A spec that already uses commas, or that is a single comparator (`^9`, `10`,
/// `>=9`), is returned unchanged. This is a syntactic bridge only — it does not
/// translate the `||` OR operator (unsupported by Cargo's `semver`).
pub(crate) fn normalize_range(spec: &str) -> String {
    let spec = spec.trim();
    // Single token, or the user already comma-separated → nothing to do.
    if spec.contains(',') || !spec.contains(char::is_whitespace) {
        return spec.to_string();
    }
    spec.split_whitespace().collect::<Vec<_>>().join(", ")
}

/// Reject a registry-declared version string before it becomes a STORE PATH
/// component (and a `.tmp-<version>-…` work-dir name) that nub then EXECUTES. The
/// dist-tag branch of [`resolve_dist`] passes a raw, registry-controlled
/// `dist-tags.<tag>` value straight into [`dist_from_meta`], and the runnable
/// target is built `<store>/pm/<pm>/<version>/package/<bin>` via `Path::join`,
/// which an absolute or `..`-laden version escapes (the F0c registry-exec
/// boundary). Mirrors the engine's own guard `aube_store::validate_version`
/// (nub-core has no aube dep, so the char-blocklist is restated here, the same
/// way [`safe_bin_subpath`] mirrors aube's bin-path guard): reject path
/// separators on any platform, NUL, control chars, and the `.`/`..` dir aliases.
///
/// nub-core is deliberately STRICTER than aube-store on one byte — it also rejects
/// `:`. The two guards have different input domains. aube-store's `version` slot
/// can carry non-semver specs (git URLs, npm aliases, file specs) that legitimately
/// contain `:`, so blocking it there would break real installs. nub-core's
/// `version` is always a CONCRETE PUBLISHED npm version: every branch of
/// [`resolve_dist`] passes a `versions[..]` KEY (npm publishes are semver), which
/// never contains `:`. Blocking it here costs nothing and closes a real escape — on
/// Windows `Path::join` treats a drive-prefixed component (`C:foo`) as drive-relative
/// and DISCARDS the base, so `<store>/pm/<pm>` joined with `C:foo` resolves to
/// `C:foo` (CWD on drive C:), relocating the written + executed PM bin outside the
/// store. `/` and `\` were already blocked, so `:` was the last Windows
/// separator-equivalent the blocklist missed.
/// A normal semver — `9.5.0`, `11.0.0-rc.1` — passes untouched.
fn validate_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 256 {
        return false;
    }
    if version
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b'/' | b'\\' | b':' | b'\0'))
    {
        return false;
    }
    !matches!(version, "." | "..")
}

/// Build a [`VersionDist`] from one `versions[X.Y.Z]` entry. `version` is the
/// resolved key (so callers print the concrete version, never the spec).
fn dist_from_meta(version: &str, meta: &Value) -> Result<VersionDist> {
    // The sole construction point of `VersionDist`, so the single chokepoint for
    // the version string before it flows into a store path. The dist-tag branch
    // feeds a raw registry value here; reject anything that could escape the join.
    if !validate_version(version) {
        bail!(
            "registry returned an unsafe version string {version:?} — refusing to use it as a store path"
        );
    }
    let dist = meta
        .get("dist")
        .with_context(|| format!("version {version} has no \"dist\" object"))?;
    let tarball = dist
        .get("tarball")
        .and_then(Value::as_str)
        .with_context(|| format!("version {version} has no dist.tarball"))?
        .to_string();
    let integrity = parse_integrity(dist)
        .with_context(|| format!("version {version} has no usable dist integrity"))?;
    let bin_subpath = bin_subpath(meta)
        .with_context(|| format!("version {version} has no resolvable bin entry to run"))?;
    Ok(VersionDist {
        version: version.to_string(),
        tarball,
        integrity,
        bin_subpath,
    })
}

/// Prefer sha512 from the SRI `dist.integrity` (it may list several algorithms
/// space-separated — pick the sha512 entry), then fall back to the hex
/// `dist.shasum` (sha1).
fn parse_integrity(dist: &Value) -> Option<Integrity> {
    if let Some(sri) = dist.get("integrity").and_then(Value::as_str) {
        if let Some(sha512) = sri
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("sha512-"))
        {
            return Some(Integrity::Sha512(sha512.to_string()));
        }
    }
    dist.get("shasum")
        .and_then(Value::as_str)
        .map(|hex| Integrity::Sha1(hex.to_string()))
}

/// The bin path to run, relative to the package root. npm's `bin` is either a
/// string (single bin == the package name) or a map of `name -> path`; for a PM
/// the entry whose key matches the package `name` is the launcher, with the
/// sole-entry and single-string forms as fallbacks. Works on a packument
/// `versions[X.Y.Z]` entry and on an installed `package/package.json` alike —
/// both carry the same `name` + `bin` shape (the cache-first path reads the
/// latter to avoid the network).
pub(crate) fn bin_subpath(meta: &Value) -> Option<PathBuf> {
    let bin = meta.get("bin")?;
    if let Some(path) = bin.as_str() {
        return safe_bin_subpath(path);
    }
    let map = bin.as_object()?;
    let name = meta.get("name").and_then(Value::as_str);
    let chosen = name
        .and_then(|n| map.get(n))
        .or_else(|| (map.len() == 1).then(|| map.values().next()).flatten())?;
    safe_bin_subpath(chosen.as_str()?)
}

/// Defense-in-depth gate on a registry-declared bin path before it becomes an
/// EXECUTED target. The packument's `bin` is attacker-controlled under a
/// compromised/MITM'd registry, and the runnable path is built as
/// `<store>/<version>/package/<bin_subpath>` then run; because `Path::join`
/// discards the base on an absolute component, an absolute entry (`/bin/sh`) or
/// one with `..` traversal would point the executed target OUTSIDE the package
/// dir, gated only by `is_file()`. Reject (drop to `None` — a malformed/malicious
/// bin surfaces as the consumers' existing "no resolvable bin" error) rather than
/// silently rewriting the path. The `Component` scan is stricter than
/// `Path::is_absolute` and platform-correct: it also catches a Windows
/// drive-relative (`C:foo` → `Prefix`) or root-relative (`\foo` → `RootDir`)
/// entry that `is_absolute()` misses, and rejects any `..` segment.
fn safe_bin_subpath(raw: &str) -> Option<PathBuf> {
    let path = PathBuf::from(raw);
    let escapes = path.components().any(|c| {
        matches!(
            c,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    });
    (!escapes && !path.as_os_str().is_empty()).then_some(path)
}

/// The bin path of a NAMED entry in a `bin` map (`npx`, `pnpx`, `yarnpkg`) —
/// the shim's seam for a package's SIBLING launchers, where [`bin_subpath`]
/// picks the entry named for the package itself (see `shim::sibling_bin`).
/// The string form declares a single bin named after the package, so it
/// matches only when `entry` IS the package name. Works on a packument
/// `versions[X.Y.Z]` entry and an installed `package/package.json` alike.
pub(crate) fn named_bin_subpath(meta: &Value, entry: &str) -> Option<PathBuf> {
    let bin = meta.get("bin")?;
    if let Some(path) = bin.as_str() {
        if meta.get("name").and_then(Value::as_str) != Some(entry) {
            return None;
        }
        return safe_bin_subpath(path);
    }
    safe_bin_subpath(bin.as_object()?.get(entry)?.as_str()?)
}

/// Networked wrapper over a bare base URL (no auth): fetch the packument from
/// `base` and resolve `spec` against it. `pkg` is the package name (`pnpm`, `npm`,
/// `yarn`). Retained for the no-auth `nub pm use` caller; provisioning goes through
/// [`resolve_version_authed`], which carries the host auth and rewrites the tarball
/// origin onto a configured mirror.
pub fn resolve_version(base: &str, pkg: &str, spec: &str) -> Result<VersionDist> {
    resolve_version_authed(
        &RegistryConfig {
            base: base.trim_end_matches('/').to_string(),
            auth: None,
        },
        pkg,
        spec,
    )
}

/// npm's abbreviated ("corgi") packument media type. Same `dist-tags` /
/// `versions[]` / `dist` / `bin` shape [`resolve_dist`] consumes, at a fraction
/// of the bytes — the full `npm` packument is ~25 MB identity-encoded, the corgi
/// form ~2.4 MB. The public registry and the mainstream private ones (Verdaccio,
/// Artifactory, GitHub Packages) honor it; a registry that ignores the `Accept`
/// header serves the full document, which resolves identically.
const CORGI_ACCEPT: &str = "application/vnd.npm.install-v1+json";

/// Networked wrapper: resolve `spec` against `cfg.base` (presenting `cfg.auth`
/// to an auth-required mirror). `pkg` is the package name (`pnpm`, `npm`,
/// `yarn`). The resolved `dist.tarball` is rewritten onto the configured
/// registry's origin ([`rewrite_tarball_origin`]) so a mirrored install fetches
/// the tarball from the same host the metadata came from, not a hardcoded
/// public URL.
///
/// An exact version or dist-tag resolves via `GET /{pkg}/{spec}` — the
/// registry's version-manifest endpoint, a few KB — because the shim's dynamic
/// default re-resolves `latest` on real invocations and paying a whole packument
/// per call was the dominant cost of #491 (25 MB, uncompressed, per `npx` run).
/// Only a RANGE needs the version-enumerating packument, fetched with the corgi
/// `Accept`. A registry that doesn't implement the version endpoint — an HTTP
/// error, or a 200 whose body isn't a usable manifest (a path-prefix proxy
/// serving the whole packument, an HTML error page) — falls back to the
/// packument, so no registry the packument path handled regresses. A TRANSPORT
/// failure (host unreachable) propagates instead, since the packument fetch
/// would only re-run the same doomed connect (and take its own retries doing it).
pub fn resolve_version_authed(cfg: &RegistryConfig, pkg: &str, spec: &str) -> Result<VersionDist> {
    let spec = spec.trim();
    let base = cfg.base.trim_end_matches('/');
    let is_exact = semver::Version::parse(spec).is_ok();
    let is_range = !is_exact && semver::VersionReq::parse(&normalize_range(spec)).is_ok();
    if !is_range {
        let url = format!("{base}/{pkg}/{spec}");
        match download::fetch_text_accept_auth(&url, None, cfg.auth.as_ref()) {
            Ok(body) => {
                if let Ok(meta) = serde_json::from_str::<Value>(&body)
                    && let Ok(mut dist) = resolve_dist_from_version_manifest(&meta)
                {
                    dist.tarball = rewrite_tarball_origin(&dist.tarball, &cfg.base);
                    return Ok(dist);
                }
                // 200 but not a version manifest → packument fallback below.
            }
            Err(e) if e.status.is_none() => {
                return Err(e.error).with_context(|| format!("fetching {url}"));
            }
            Err(_) => {} // endpoint unsupported/404 on this registry → packument
        }
    }
    let url = format!("{base}/{pkg}");
    let body = download::fetch_text_accept_auth(&url, Some(CORGI_ACCEPT), cfg.auth.as_ref())
        .map_err(|e| e.error)
        .with_context(|| format!("fetching packument {url}"))?;
    let packument: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing packument {url}"))?;
    let mut dist =
        resolve_dist(&packument, spec).with_context(|| format!("resolving {pkg}@{spec}"))?;
    dist.tarball = rewrite_tarball_origin(&dist.tarball, &cfg.base);
    Ok(dist)
}

/// Build a [`VersionDist`] from a VERSION-MANIFEST document (`GET
/// /{pkg}/{version-or-tag}`) — the same `name`/`bin`/`dist` shape as one
/// packument `versions[]` entry, plus its own top-level `version`. PURE — no
/// network — so the parsing is unit-tested offline. Routes through
/// [`dist_from_meta`], the single `VersionDist` chokepoint, so the
/// store-path guard ([`validate_version`]) and the bin-path guard apply to this
/// server-controlled document exactly as to a packument.
pub fn resolve_dist_from_version_manifest(meta: &Value) -> Result<VersionDist> {
    let version = meta
        .get("version")
        .and_then(Value::as_str)
        .context("version manifest has no \"version\" field")?;
    dist_from_meta(version, meta)
}

/// Verify a downloaded tarball against its dist integrity. Fail-closed: a mismatch
/// (or an unreadable file) is an error, and the caller verifies BEFORE extracting.
/// sha512 is checked when present; sha1 only for publishes that lack `integrity`.
///
/// The expected sha512 is the registry's base64 SRI payload — decoded to raw
/// bytes and compared against the raw digest, so there's no base64-vs-base64
/// canonicalization risk (and no base64 *encoder* dependency).
pub fn verify_integrity(file: &std::path::Path, want: &Integrity) -> Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    match want {
        Integrity::Sha512(expected_b64) => {
            let expected = base64_decode(expected_b64)
                .with_context(|| format!("decoding sha512 SRI for {}", file.display()))?;
            let got = Sha512::digest(&bytes);
            if got.as_slice() != expected.as_slice() {
                bail!(
                    "sha512 integrity mismatch for {}: expected sha512-{expected_b64}, got sha512-{}",
                    file.display(),
                    base64_encode(&got)
                );
            }
        }
        Integrity::Sha1(expected_hex) => {
            let got = super::hex_lower(&Sha1::digest(&bytes));
            if !got.eq_ignore_ascii_case(expected_hex) {
                bail!(
                    "sha1 integrity mismatch for {}: expected {expected_hex}, got {got}",
                    file.display()
                );
            }
        }
    }
    Ok(())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard-alphabet base64 decode (the SRI form npm publishes — padded, `+/`).
/// Small and self-contained so verification needs no base64 *crate*; only the
/// alphabet npm uses is accepted (`-_` URL-safe or stray chars are rejected).
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for ch in s.bytes() {
        let val = B64
            .iter()
            .position(|&c| c == ch)
            .with_context(|| format!("invalid base64 character {:?}", ch as char))?
            as u32;
        acc = (acc << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Standard-alphabet base64 encode — only used to render the *actual* digest in a
/// mismatch message (the happy path never encodes).
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - i * 6)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small packument in the real registry shape: a `dist-tags` map and a
    /// `versions` object whose entries carry `dist.{tarball,integrity,shasum}` and
    /// `bin`. Mixed integrity coverage: 10.0.0 has sha512, 8.0.0 is sha1-only.
    const PACKUMENT: &str = r#"{
        "name": "pnpm",
        "dist-tags": { "latest": "10.0.0", "next": "11.0.0-rc.1" },
        "versions": {
            "8.0.0": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs", "pnpx": "bin/pnpx.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-8.0.0.tgz",
                    "shasum": "0123456789abcdef0123456789abcdef01234567"
                }
            },
            "9.5.0": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-9.5.0.tgz",
                    "integrity": "sha512-AAAA",
                    "shasum": "aaaa"
                }
            },
            "10.0.0": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-10.0.0.tgz",
                    "integrity": "sha512-BBBB",
                    "shasum": "bbbb"
                }
            },
            "11.0.0-rc.1": {
                "name": "pnpm",
                "bin": { "pnpm": "bin/pnpm.cjs" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-11.0.0-rc.1.tgz",
                    "integrity": "sha512-CCCC"
                }
            }
        }
    }"#;

    fn packument() -> Value {
        serde_json::from_str(PACKUMENT).unwrap()
    }

    #[test]
    fn resolves_exact_dist_tag_and_range_to_the_right_version() {
        let p = packument();

        // Exact: verbatim key, sha512 chosen over the sibling shasum.
        let exact = resolve_dist(&p, "9.5.0").unwrap();
        assert_eq!(exact.version, "9.5.0");
        assert_eq!(exact.integrity, Integrity::Sha512("AAAA".into()));
        assert_eq!(exact.bin_subpath, PathBuf::from("bin/pnpm.cjs"));

        // dist-tag: `latest` resolves to its mapped concrete version.
        assert_eq!(resolve_dist(&p, "latest").unwrap().version, "10.0.0");

        // Range: highest satisfying STABLE key (the rc is not in range for ^9).
        assert_eq!(resolve_dist(&p, "^9").unwrap().version, "9.5.0");
        // Bare-major range picks the newest 10.x, not the 11 rc.
        assert_eq!(resolve_dist(&p, "10").unwrap().version, "10.0.0");
        // node-semver space-separated comparators (`>=9 <10`) — npm/devEngines
        // write these; Cargo's semver needs commas, so the normalizer bridges it.
        assert_eq!(resolve_dist(&p, ">=9 <10").unwrap().version, "9.5.0");
    }

    #[test]
    fn version_manifest_resolves_through_the_same_guards_as_a_packument() {
        // The version-manifest endpoint (`GET /{pkg}/{version-or-tag}`) returns
        // one versions[] entry with its own top-level "version" — the fast path
        // for exact/tag specs (#491). It must flow through dist_from_meta, the
        // VersionDist chokepoint, so the store-path guard holds.
        let meta: Value = serde_json::from_str(
            r#"{
                "name": "npm", "version": "12.0.1",
                "bin": { "npm": "bin/npm-cli.js", "npx": "bin/npx-cli.js" },
                "dist": {
                    "tarball": "https://registry.npmjs.org/npm/-/npm-12.0.1.tgz",
                    "integrity": "sha512-DDDD"
                }
            }"#,
        )
        .unwrap();
        let dist = resolve_dist_from_version_manifest(&meta).unwrap();
        assert_eq!(dist.version, "12.0.1");
        assert_eq!(dist.integrity, Integrity::Sha512("DDDD".into()));
        assert_eq!(dist.bin_subpath, PathBuf::from("bin/npm-cli.js"));

        // No "version" field → error, never a guessed store path.
        let no_version: Value = serde_json::from_str(r#"{ "name": "npm" }"#).unwrap();
        assert!(resolve_dist_from_version_manifest(&no_version).is_err());

        // A server-controlled version that would escape the store join is
        // refused — same F0c posture as the packument dist-tag branch.
        let escaping: Value = serde_json::from_str(
            r#"{ "name": "npm", "version": "..", "bin": "bin/npm-cli.js",
                 "dist": { "tarball": "https://x/t.tgz", "integrity": "sha512-EEEE" } }"#,
        )
        .unwrap();
        let err = resolve_dist_from_version_manifest(&escaping)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unsafe version string"),
            "escaping version must be refused: {err}"
        );
    }

    #[test]
    fn normalize_range_bridges_space_separated_comparators_only() {
        // Space-separated comparators → comma form (the only translation).
        assert_eq!(normalize_range(">=9 <11"), ">=9, <11");
        assert_eq!(normalize_range(">=9   <11"), ">=9, <11"); // runs of space collapse
        // Single comparators and bare versions pass through untouched.
        assert_eq!(normalize_range("^9"), "^9");
        assert_eq!(normalize_range("10"), "10");
        // An already-comma'd spec is left alone (no double-comma).
        assert_eq!(normalize_range(">=9, <11"), ">=9, <11");
    }

    #[test]
    fn nonexistent_exact_and_unsatisfiable_range_error() {
        let p = packument();
        // An exact version that isn't published isn't a valid range either, so it
        // surfaces the range parse/resolution error rather than a doomed fetch.
        assert!(resolve_dist(&p, "9.9.9").is_err());
        // A range with no matching key errors naming the spec.
        let err = resolve_dist(&p, ">=20").unwrap_err().to_string();
        assert!(
            err.contains(">=20"),
            "error names the unsatisfiable spec: {err}"
        );
    }

    #[test]
    fn sha1_only_publish_falls_back_to_shasum() {
        // 8.0.0 has no `integrity` — the resolver must fall back to dist.shasum.
        let dist = resolve_dist(&packument(), "8.0.0").unwrap();
        assert_eq!(
            dist.integrity,
            Integrity::Sha1("0123456789abcdef0123456789abcdef01234567".into())
        );
    }

    #[test]
    fn bin_subpath_picks_the_entry_named_for_the_package() {
        // 8.0.0's bin map has two entries; the one keyed by the package name wins.
        let dist = resolve_dist(&packument(), "8.0.0").unwrap();
        assert_eq!(dist.bin_subpath, PathBuf::from("bin/pnpm.cjs"));
    }

    #[test]
    fn named_bin_subpath_picks_arbitrary_entries_but_string_form_only_the_package_name() {
        // The map form: any entry resolves by name (the npx/pnpx seam).
        let meta: Value = serde_json::json!({
            "name": "npm",
            "bin": { "npm": "bin/npm-cli.js", "npx": "bin/npx-cli.js" }
        });
        assert_eq!(
            named_bin_subpath(&meta, "npx"),
            Some(PathBuf::from("bin/npx-cli.js"))
        );
        assert_eq!(
            named_bin_subpath(&meta, "corepack"),
            None,
            "an entry the package doesn't declare is a miss, not a guess"
        );

        // The string form declares a single bin named for the PACKAGE — it
        // satisfies only that name.
        let meta: Value = serde_json::json!({ "name": "yarn", "bin": "bin/yarn.js" });
        assert_eq!(
            named_bin_subpath(&meta, "yarn"),
            Some(PathBuf::from("bin/yarn.js"))
        );
        assert_eq!(
            named_bin_subpath(&meta, "yarnpkg"),
            None,
            "a string-form bin must not satisfy a sibling entry name"
        );
    }

    #[test]
    fn bin_subpath_rejects_a_path_that_escapes_the_package_dir() {
        // The executed target is built as `<store>/<version>/package/<bin_subpath>`;
        // an absolute or `..`-laden registry `bin` would escape that dir. Both the
        // string and map forms, for both resolvers, must drop to None — never an
        // out-of-package PathBuf the caller would `is_file()`-gate and run.
        for bad in [
            "/bin/sh",
            "../../../bin/sh",
            "bin/../../escape",
            "/abs/cli.js",
        ] {
            assert_eq!(
                bin_subpath(&serde_json::json!({ "name": "pnpm", "bin": bad })),
                None,
                "string-form bin {bad:?} escapes the package dir and must be rejected"
            );
            assert_eq!(
                bin_subpath(&serde_json::json!({ "name": "pnpm", "bin": { "pnpm": bad } })),
                None,
                "map-form bin {bad:?} escapes the package dir and must be rejected"
            );
            assert_eq!(
                named_bin_subpath(
                    &serde_json::json!({ "name": "pnpm", "bin": { "pnpx": bad } }),
                    "pnpx"
                ),
                None,
                "named bin {bad:?} escapes the package dir and must be rejected"
            );
        }

        // A normal relative bin (with or without a subdir) still resolves.
        assert_eq!(
            bin_subpath(&serde_json::json!({ "name": "pnpm", "bin": "bin/pnpm.cjs" })),
            Some(PathBuf::from("bin/pnpm.cjs"))
        );
        assert_eq!(
            bin_subpath(&serde_json::json!({ "name": "cli", "bin": "cli.js" })),
            Some(PathBuf::from("cli.js"))
        );
    }

    #[test]
    fn resolve_dist_errors_gracefully_on_an_escaping_bin() {
        // The full resolver (the source feeding all three execution-target joins in
        // provision.rs) must surface a clean error — not a panic, not an
        // out-of-package PathBuf — when a version's bin escapes the package dir.
        let meta = serde_json::json!({
            "name": "pnpm",
            "versions": {
                "9.0.0": {
                    "name": "pnpm",
                    "bin": "/bin/sh",
                    "dist": {
                        "tarball": "https://example.test/pnpm-9.0.0.tgz",
                        "integrity": "sha512-deadbeef"
                    }
                }
            }
        });
        assert!(
            resolve_dist(&meta, "9.0.0").is_err(),
            "an escaping bin must fail resolution, never yield a runnable out-of-package target"
        );
    }

    #[test]
    fn validate_version_accepts_semver_and_rejects_path_escapes() {
        // Legitimate version strings the resolver must keep accepting.
        for ok in [
            "9.5.0",
            "11.0.0-rc.1",
            "1.2.3+build.5",
            "0.0.0-canary.20240101",
        ] {
            assert!(validate_version(ok), "{ok:?} is a legitimate version");
        }
        // Anything that could escape `<store>/pm/<pm>/<version>` when joined, or
        // alias a directory, must be refused (F0c).
        for bad in [
            "../evil",
            "..",
            ".",
            "a/b",
            "a\\b",
            "x\u{0}y",
            "ctrl\u{7}x",
            "",
            // F0c (Windows): a drive-prefixed version is drive-RELATIVE under
            // `Path::join` (it discards the store base), so `:` must be rejected
            // just like `/` and `\`.
            "C:foo",
            "C:",
            "C:\\windows\\system32",
            "npm:alias@1.0.0",
        ] {
            assert!(
                !validate_version(bad),
                "{bad:?} must be rejected as a store-path component"
            );
        }
    }

    #[test]
    fn resolve_dist_rejects_a_dist_tag_pointing_at_a_path_escaping_version() {
        // F0c: a malicious / MITM registry points a dist-tag at a version KEY that
        // carries `..`. The runnable PM target is `<store>/pm/<pm>/<version>/…`
        // built with `Path::join`, so an escaping version would relocate the
        // executed bin outside the store. The dist-tag branch is the one that
        // passes a raw registry string straight to `dist_from_meta`; resolution
        // must refuse it rather than hand back a traversal-bearing `VersionDist`.
        let p: Value = serde_json::from_str(
            r#"{
                "name": "pnpm",
                "dist-tags": { "latest": "../../../../tmp/evil" },
                "versions": {
                    "../../../../tmp/evil": {
                        "name": "pnpm",
                        "bin": { "pnpm": "bin/pnpm.cjs" },
                        "dist": {
                            "tarball": "https://registry.npmjs.org/pnpm/-/pnpm-evil.tgz",
                            "integrity": "sha512-EVIL"
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let err = resolve_dist(&p, "latest").unwrap_err().to_string();
        assert!(
            err.contains("unsafe version"),
            "an escaping dist-tag version must be refused: {err}"
        );
    }

    #[test]
    fn auth_for_tarball_attaches_only_to_the_registry_origin() {
        let auth = Auth::Bearer("secret-token".into());
        let cfg = RegistryConfig {
            base: "https://npm.corp.test/api/npm".into(),
            auth: Some(auth.clone()),
        };
        // Same origin (the private-mirror case, post origin-rewrite) → auth rides.
        assert_eq!(
            auth_for_tarball(&cfg, "https://npm.corp.test/pnpm/-/pnpm-10.0.0.tgz"),
            Some(&auth)
        );
        // A foreign-host tarball a malicious/MITM packument named must NOT receive
        // the registry credential (N1b — the leak this guard closes).
        assert_eq!(
            auth_for_tarball(&cfg, "https://evil.test/pnpm/-/pnpm-10.0.0.tgz"),
            None
        );
        // An https→http downgrade to the same host is a different origin → no auth
        // (belt-and-suspenders with the redirect-policy downgrade stop).
        assert_eq!(
            auth_for_tarball(&cfg, "http://npm.corp.test/pnpm/-/pnpm-10.0.0.tgz"),
            None
        );
        // No auth configured (the public-registry default) → nothing to leak.
        let no_auth = RegistryConfig {
            base: PUBLIC_REGISTRY.into(),
            auth: None,
        };
        assert_eq!(
            auth_for_tarball(&no_auth, "https://registry.npmjs.org/x/-/x-1.0.0.tgz"),
            None
        );
    }

    #[test]
    fn registry_base_reads_npmrc_and_normalizes_the_trailing_slash() {
        // The `npm_config_registry` branch is process-global env (flaky to mutate
        // under the parallel harness) — covered by the documented single-override
        // shape, not asserted here. When it's set in the ambient env, skip the
        // lower-precedence assertions it would shadow.
        let env_set = std::env::var("npm_config_registry").is_ok_and(|v| !v.trim().is_empty());

        let dir = std::env::temp_dir().join(format!("nub-pm-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A project `.npmrc registry` is read and its trailing slash trimmed.
        std::fs::write(dir.join(".npmrc"), "registry=https://r.example.test/\n").unwrap();
        if !env_set {
            assert_eq!(registry_base(&dir), "https://r.example.test");
        }

        // No project key and no env → the public registry, slash trimmed.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        if !env_set && npmrc_value(&empty, "registry").is_none() {
            assert_eq!(registry_base(&empty), "https://registry.npmjs.org");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_integrity_is_fail_closed_and_prefers_sha512() {
        let dir = std::env::temp_dir().join(format!("nub-pm-int-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob");
        std::fs::write(&f, b"abc").unwrap();

        // Precomputed digests of "abc":
        //   sha512(base64) = the canonical "abc" SHA-512, base64-encoded
        //   sha1(hex)      = a9993e364706816aba3e25717850c26c9cd0d89d
        let sha512_abc = "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==";
        let sha1_abc = "a9993e364706816aba3e25717850c26c9cd0d89d";

        assert!(
            verify_integrity(&f, &Integrity::Sha512(sha512_abc.into())).is_ok(),
            "matching sha512 verifies"
        );
        assert!(
            verify_integrity(&f, &Integrity::Sha1(sha1_abc.into())).is_ok(),
            "matching sha1 verifies (uppercase-tolerant)"
        );

        // A wrong sha512 must fail, and the message must carry both digests so a CI
        // failure is self-debugging.
        let err = verify_integrity(&f, &Integrity::Sha512("WRONG".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("expected sha512-WRONG"),
            "names the expected: {err}"
        );
        assert!(err.contains(sha512_abc), "names the actual digest: {err}");

        assert!(
            verify_integrity(&f, &Integrity::Sha1("dead".into())).is_err(),
            "a wrong sha1 fails closed"
        );

        // An expected sha512 SRI payload with a non-base64 character can't decode;
        // that must fail closed at decode time, not pass verification. The message
        // names the decode context so a CI failure points at the malformed SRI.
        let err = verify_integrity(&f, &Integrity::Sha512("!!not-base64!!".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("decoding sha512 SRI"),
            "an undecodable SRI must fail closed at decode, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
