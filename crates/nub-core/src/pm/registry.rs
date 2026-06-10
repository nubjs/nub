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

use std::path::PathBuf;

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
    let mut by_prefix: std::collections::HashMap<String, Fields> = std::collections::HashMap::new();

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

/// Build a [`VersionDist`] from one `versions[X.Y.Z]` entry. `version` is the
/// resolved key (so callers print the concrete version, never the spec).
fn dist_from_meta(version: &str, meta: &Value) -> Result<VersionDist> {
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
        return Some(PathBuf::from(path));
    }
    let map = bin.as_object()?;
    let name = meta.get("name").and_then(Value::as_str);
    let chosen = name
        .and_then(|n| map.get(n))
        .or_else(|| (map.len() == 1).then(|| map.values().next()).flatten())?;
    chosen.as_str().map(PathBuf::from)
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
        return (meta.get("name").and_then(Value::as_str) == Some(entry))
            .then(|| PathBuf::from(path));
    }
    bin.as_object()?.get(entry)?.as_str().map(PathBuf::from)
}

/// Networked wrapper over a bare base URL (no auth): fetch the packument from
/// `base` and resolve `spec` against it. `pkg` is the package name (`pnpm`, `npm`,
/// `yarn`). Retained for the no-auth `nub pm pin` caller; provisioning goes through
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

/// Networked wrapper: fetch the packument from `cfg.base` (presenting `cfg.auth`
/// to an auth-required mirror) and resolve `spec` against it. `pkg` is the package
/// name (`pnpm`, `npm`, `yarn`). The resolved `dist.tarball` is rewritten onto the
/// configured registry's origin ([`rewrite_tarball_origin`]) so a mirrored install
/// fetches the tarball from the same host the packument came from, not a hardcoded
/// public URL.
pub fn resolve_version_authed(cfg: &RegistryConfig, pkg: &str, spec: &str) -> Result<VersionDist> {
    let url = format!("{}/{pkg}", cfg.base.trim_end_matches('/'));
    let body = download::fetch_text_auth(&url, cfg.auth.as_ref())
        .with_context(|| format!("fetching packument {url}"))?;
    let packument: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing packument {url}"))?;
    let mut dist =
        resolve_dist(&packument, spec).with_context(|| format!("resolving {pkg}@{spec}"))?;
    dist.tarball = rewrite_tarball_origin(&dist.tarball, &cfg.base);
    Ok(dist)
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
