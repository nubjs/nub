use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(super) struct RawBunLockfile {
    #[serde(rename = "lockfileVersion")]
    pub(super) lockfile_version: u32,
    /// bun 1.2+ emits a `configVersion:` field alongside
    /// `lockfileVersion:`. Default to `1` for older lockfiles that
    /// predate it so a v1.1 file round-trips without the field
    /// suddenly appearing.
    #[serde(default = "default_config_version", rename = "configVersion")]
    pub(super) config_version: u32,
    #[serde(default)]
    pub(super) workspaces: BTreeMap<String, RawBunWorkspace>,
    #[serde(default)]
    pub(super) packages: BTreeMap<String, Vec<serde_json::Value>>,
    /// bun 1.1+ top-level `overrides:` block (mirrors the key under
    /// the same name in package.json). Map of selector → version.
    #[serde(default)]
    pub(super) overrides: BTreeMap<String, String>,
    /// bun 1.1+ top-level `patchedDependencies:` block. Map of
    /// `pkg@version` selector → relative patch file path.
    #[serde(default, rename = "patchedDependencies")]
    pub(super) patched_dependencies: BTreeMap<String, String>,
    /// bun 1.1+ top-level `trustedDependencies:` — a package-name
    /// allowlist for lifecycle script execution.
    #[serde(default, rename = "trustedDependencies")]
    pub(super) trusted_dependencies: Vec<String>,
    /// bun 1.2+ unnamed catalog (`catalog: { foo: "^1.0.0" }`).
    /// Pairs with pnpm's `catalog:` in `pnpm-workspace.yaml`.
    #[serde(default)]
    pub(super) catalog: BTreeMap<String, String>,
    /// bun 1.2+ named catalogs (`catalogs: { evens: { foo: "^2" } }`).
    #[serde(default)]
    pub(super) catalogs: BTreeMap<String, BTreeMap<String, String>>,
    /// Unknown top-level fields preserved verbatim so a future bun
    /// bump (or anything hand-authored we don't model) round-trips
    /// without getting silently stripped.
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, serde_json::Value>,
}

fn default_config_version() -> u32 {
    1
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawBunWorkspace {
    #[serde(default)]
    pub(super) dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) optional_dependencies: BTreeMap<String, String>,
    /// Unknown per-workspace fields (`name`, `version`, `bin`,
    /// `peerDependencies`, `optionalPeers`, and anything else bun
    /// adds in a future release) preserved verbatim. The writer's
    /// ws-extras fallback re-emits them so bun-authored workspace
    /// peer data round-trips even when the manifest isn't
    /// authoritative for the importer.
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, serde_json::Value>,
}

/// Decoded view of one bun.lock package entry.
///
/// bun uses different tuple shapes depending on where the package came
/// from:
///   - Registry: `[ident, resolved_url, { meta }, "sha512-..."]`
///   - Git / github: `[ident, { meta }, "owner-repo-commit"]`
///   - Workspace / link / file: `[ident]` or `[ident, { meta }]`
///
/// We introspect by element type rather than position: the metadata
/// object is the sole `Object` in the array, and an integrity hash is
/// recognized by its `sha…-` prefix.
#[derive(Debug, Default)]
pub(super) struct BunEntry {
    pub(super) ident: String,
    pub(super) meta: RawBunMeta,
    pub(super) integrity: Option<String>,
    /// Registry tuple slot 1 — the resolved registry/tarball URL bun
    /// writes for an npm package installed from a non-default registry
    /// (`[ident, "<url>", {meta}, integrity]`). bun emits `""` for the
    /// default registry, so an empty slot means "default" and is
    /// dropped here. Only the registry shape carries a URL string
    /// *before* the meta object; a git/github entry's third element is
    /// the `owner-repo-sha` repo-tag, which sits *after* the meta object
    /// and must not be mistaken for a registry URL. Re-emitting this on
    /// round-trip is what keeps a scoped/private-registry bun.lock from
    /// silently re-routing to the default npm registry on the next
    /// resolve.
    pub(super) registry_url: Option<String>,
}

impl BunEntry {
    pub(super) fn from_array(key: &str, arr: &[serde_json::Value]) -> Result<Self, String> {
        let ident = arr
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("package '{key}' has no ident string at position 0"))?
            .to_string();

        let mut meta = RawBunMeta::default();
        let mut integrity: Option<String> = None;
        let mut registry_url: Option<String> = None;
        let mut seen_meta = false;
        for el in arr.iter().skip(1) {
            match el {
                serde_json::Value::Object(_) => {
                    meta = serde_json::from_value(el.clone()).unwrap_or_default();
                    seen_meta = true;
                }
                serde_json::Value::String(s) if is_integrity_hash(s) => {
                    integrity = Some(s.clone());
                }
                // The registry URL is the lone non-integrity string that
                // precedes the meta object (slot 1 of the npm tuple). A
                // git/github repo-tag is also a non-integrity string but
                // follows the meta object, so gate on `!seen_meta`. An
                // empty slot is bun's "default registry" marker — leave
                // it `None` so re-emit writes `""` exactly as bun does.
                serde_json::Value::String(s) if !seen_meta && !s.is_empty() => {
                    registry_url = Some(s.clone());
                }
                _ => {}
            }
        }

        Ok(Self {
            ident,
            meta,
            integrity,
            registry_url,
        })
    }
}

/// Recognize an SRI-style integrity hash (`<algo>-<base64>`).
///
/// The prefix check alone isn't enough: a github entry's trailing
/// `owner-repo-shortsha` could start with a literal `sha1`/`sha256`/etc.
/// if that's the owner name. A real SRI hash also has a fixed base64
/// body length for each algo, and base64 never uses `-`, so
/// `sha1-myrepo-abc123` fails both the length and charset checks.
pub(super) fn is_integrity_hash(s: &str) -> bool {
    let Some((algo, body)) = s.split_once('-') else {
        return false;
    };
    // Accept sha1 and md5 at the parser layer so bun lockfiles that
    // reference pre-2017 npm packages (whose `dist.integrity` is only
    // ever sha1) still round-trip without losing the hash. Downgrade
    // enforcement lives at verify time in `aube-store::verify_integrity`,
    // which already refuses anything but sha512 for content verification.
    let expected_len = match algo {
        "sha512" => 88,
        "sha384" => 64,
        "sha256" => 44,
        "sha1" => 28,
        "md5" => 24,
        _ => return false,
    };
    if body.len() != expected_len {
        return false;
    }
    body.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawBunMeta {
    #[serde(default)]
    pub(super) dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) optional_dependencies: BTreeMap<String, String>,
    /// bun records peer declarations on the meta block in the same
    /// shape as `dependencies`. Keeping them typed lets the writer
    /// emit them back in bun's native field order; anything we don't
    /// have an explicit slot for drops through to `extra` below.
    #[serde(default)]
    pub(super) peer_dependencies: BTreeMap<String, String>,
    /// Compact list form of `peerDependenciesMeta[name].optional:
    /// true` — bun's preferred representation on per-entry meta.
    #[serde(default)]
    pub(super) optional_peers: Vec<String>,
    /// `bin:` map — bun records executables by name on each package's
    /// meta block (`{ "bin": { "semver": "bin/semver.js" } }`). Round-
    /// tripping it is what keeps `aube install --no-frozen-lockfile`
    /// from silently dropping the `bin:` line and drifting against
    /// bun's own output.
    #[serde(default)]
    pub(super) bin: serde_json::Value,
    /// Platform filters — bun writes arrays of `os` / `cpu` / `libc`
    /// entries on meta blocks for optional platform packages.
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) libc: Vec<String>,
    /// Unknown per-entry meta fields preserved for round-trip
    /// (`deprecated`, `hasInstallScript`, anything new bun adds).
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, serde_json::Value>,
}
