pub mod dev_engines;
pub mod workspace;

pub use dev_engines::{DevEngineDependency, DevEngines, OnFail, dev_engines_tolerant};
pub use workspace::{JailBuildPermission, WorkspaceConfig};

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Deserialize `engines` tolerant to legacy non-map forms, e.g.
/// `extsprintf@1.4.1` ships `"engines": ["node >=0.6.0"]` and some
/// old packument entries (such as `qs`) ship a bare string.
/// Modern npm ignores that shape (engine-strict only consults the map
/// form), so normalize to an empty map rather than failing the whole
/// manifest — a hard error there takes down every install that touches
/// one of these ancient packages, even when the user's target engine
/// wouldn't have matched any constraint anyway.
///
/// An explicit `null` is also tolerated (same as "field absent"),
/// matching the tolerance our other dep-map parsers apply.
///
/// Exposed (`pub`) so the lockfile parser can apply the same tolerance
/// — npm v2/v3 lockfiles preserve the array shape verbatim from the
/// originating `package.json`, so a strict map-only deserializer there
/// trips on the same ancient packages and blocks `aube ci` outright.
pub fn engines_tolerant<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(match value {
        None
        | Some(serde_json::Value::Null)
        | Some(serde_json::Value::Array(_))
        | Some(serde_json::Value::String(_)) => BTreeMap::new(),
        Some(serde_json::Value::Object(m)) => m
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        Some(other) => {
            // Null / Array / String / Object are handled above, so
            // `other` can only be another scalar here.
            return Err(serde::de::Error::custom(format!(
                "engines: expected a map, got {}",
                match other {
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "boolean",
                    _ => unreachable!("engines: unexpected value variant"),
                }
            )));
        }
    })
}

/// Deserialize `scripts` tolerant to non-string values. `firefox-profile`
/// (and a handful of other legacy packages) ships junk like
/// `"scripts": { "blanket": { "pattern": [...] } }` — tool-specific
/// config that npm's CLI treats as "not a runnable script" and ignores.
/// A strict `Record<string, string>` deserialization trips on the object
/// entry and fails the whole install. Drop non-string entries so the
/// real scripts still round-trip.
pub fn scripts_tolerant<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => BTreeMap::new(),
        Some(serde_json::Value::Object(m)) => m
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        Some(_) => BTreeMap::new(),
    })
}

/// Tolerant dep-map deserializer. Same shape as scripts_tolerant
/// but used for dependencies / devDependencies / peerDependencies /
/// optionalDependencies. Real world manifests written by tools
/// sometimes emit `"peerDependencies": null` when a package has
/// none, and strict Record<string, string> deserialization rejects
/// that. npm and pnpm both tolerate null. Drop non-string values
/// (numbers, arrays, objects) silently since nothing sensible maps
/// those to a version range.
pub fn deps_tolerant<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => BTreeMap::new(),
        Some(serde_json::Value::Object(m)) => m
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        Some(_) => BTreeMap::new(),
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_dependencies: Vec<String>,
}

/// Parsed `package.json`.
///
/// Deserializes via [`PackageJsonRaw`] (`#[serde(from = ...)]`) so a
/// manifest carrying *both* `bundledDependencies` and the deprecated
/// `bundleDependencies` alias parses without tripping serde's duplicate
/// field check. Some real-world publishes (e.g. `@lingui/message-utils`)
/// ship both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "PackageJsonRaw")]
pub struct PackageJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(
        default,
        deserialize_with = "deps_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub dependencies: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "deps_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "deps_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "deps_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_config: Option<UpdateConfig>,
    /// `bundledDependencies` from package.json. Names listed here are
    /// shipped *inside* the package tarball itself, under the package's
    /// own `node_modules/`. The resolver must not recurse into them, and
    /// Node's directory walk serves them straight out of the extracted
    /// tree. On deserialize we also accept the deprecated
    /// `bundleDependencies` spelling and prefer the canonical when both
    /// are present (handled in [`PackageJsonRaw`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_dependencies: Option<BundledDependencies>,
    #[serde(
        default,
        deserialize_with = "scripts_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub scripts: BTreeMap<String, String>,
    /// `engines` field — declared runtime version constraints, e.g.
    /// `{"node": ">=18.0.0"}`. Checked against the current runtime during
    /// `aube install`; a mismatch warns by default and fails under
    /// `engine-strict`. See `engines_tolerant` for the legacy-shape
    /// handling.
    #[serde(
        default,
        deserialize_with = "engines_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub engines: BTreeMap<String, String>,
    /// `devEngines` field — development-environment requirements per
    /// the OpenJS spec. aube acts on `devEngines.runtime` entries named
    /// `node` (version switching); see [`dev_engines`] for the shape
    /// and tolerance rules.
    #[serde(
        default,
        deserialize_with = "dev_engines_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub dev_engines: Option<DevEngines>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Workspaces>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Deserialize-only mirror of [`PackageJson`] that splits the
/// `bundled_dependencies` field into two name-distinct slots so a
/// manifest carrying *both* `bundledDependencies` and `bundleDependencies`
/// doesn't trip serde's duplicate field check the way `#[serde(alias)]`
/// does. The canonical spelling wins on merge.
///
/// **Maintenance invariant:** every non-`bundled_dependencies` field
/// here must mirror its counterpart on [`PackageJson`] *byte-for-byte*
/// in serde attributes (`rename`, `deserialize_with`, `default`,
/// `flatten`, etc.). The `From` impl below catches missing fields at
/// compile time, but **attribute drift is silent** — e.g. forgetting
/// `deserialize_with = "deps_tolerant"` here would make the deserialize
/// path strict on dep-map shapes the public type silently tolerates.
/// When adding or modifying a field on `PackageJson`, update this
/// struct in lockstep.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageJsonRaw {
    name: Option<String>,
    version: Option<String>,
    #[serde(default, deserialize_with = "deps_tolerant")]
    dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deps_tolerant")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deps_tolerant")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deps_tolerant")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    update_config: Option<UpdateConfig>,
    #[serde(default, rename = "bundledDependencies")]
    bundled_dependencies: Option<BundledDependencies>,
    #[serde(default, rename = "bundleDependencies")]
    bundle_dependencies_alias: Option<BundledDependencies>,
    #[serde(default, deserialize_with = "scripts_tolerant")]
    scripts: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "engines_tolerant")]
    engines: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "dev_engines_tolerant")]
    dev_engines: Option<DevEngines>,
    #[serde(default)]
    workspaces: Option<Workspaces>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl From<PackageJsonRaw> for PackageJson {
    fn from(raw: PackageJsonRaw) -> Self {
        Self {
            name: raw.name,
            version: raw.version,
            dependencies: raw.dependencies,
            dev_dependencies: raw.dev_dependencies,
            peer_dependencies: raw.peer_dependencies,
            optional_dependencies: raw.optional_dependencies,
            update_config: raw.update_config,
            bundled_dependencies: raw.bundled_dependencies.or(raw.bundle_dependencies_alias),
            scripts: raw.scripts,
            engines: raw.engines,
            dev_engines: raw.dev_engines,
            workspaces: raw.workspaces,
            extra: raw.extra,
        }
    }
}

/// `bundledDependencies` shape from package.json. npm/pnpm accept
/// either an array of dep names or a boolean (`true` meaning "bundle
/// everything in `dependencies`"). We preserve both so the resolver
/// can compute the exact name set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum BundledDependencies {
    List(Vec<String>),
    All(bool),
}

impl BundledDependencies {
    /// The set of dep names that should be treated as bundled, given
    /// the package's own `dependencies` map (needed for the `true`
    /// form, which means "bundle every production dep").
    pub fn names<'a>(&'a self, dependencies: &'a BTreeMap<String, String>) -> Vec<&'a str> {
        match self {
            BundledDependencies::List(v) => v.iter().map(String::as_str).collect(),
            BundledDependencies::All(true) => dependencies.keys().map(String::as_str).collect(),
            BundledDependencies::All(false) => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Workspaces {
    /// Bare single-pattern form. npm accepts
    /// `"workspaces": "packages/*"` even though the docs only show
    /// the array form. Some bun projects in the wild use it too.
    /// Without this, those manifests fail to parse and user gets a
    /// cryptic serde error pointing at the string.
    String(String),
    Array(Vec<String>),
    Object {
        // `packages` stays required (no `#[serde(default)]`) so that a
        // typo like `"pacakges"` fails deserialization instead of
        // silently producing an empty vec. Bun's object form always
        // includes `packages`, so this doesn't lock out the catalog use
        // case.
        packages: Vec<String>,
        #[serde(default)]
        nohoist: Vec<String>,
        /// Bun-style default catalog nested under `workspaces.catalog`.
        /// Aube reads it in addition to `pnpm-workspace.yaml`'s `catalog:`
        /// so bun projects that migrated config into package.json keep
        /// working.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        catalog: BTreeMap<String, String>,
        /// Bun-style named catalogs nested under `workspaces.catalogs`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        catalogs: BTreeMap<String, BTreeMap<String, String>>,
    },
}

impl Workspaces {
    pub fn patterns(&self) -> &[String] {
        match self {
            Workspaces::String(s) => std::slice::from_ref(s),
            Workspaces::Array(v) => v,
            Workspaces::Object { packages, .. } => packages,
        }
    }

    /// Bun-style default catalog (`workspaces.catalog`). Empty when the
    /// `workspaces` field is an array or the object form has no catalog.
    pub fn catalog(&self) -> &BTreeMap<String, String> {
        static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
        match self {
            Workspaces::String(_) | Workspaces::Array(_) => EMPTY.get_or_init(BTreeMap::new),
            Workspaces::Object { catalog, .. } => catalog,
        }
    }

    /// Bun-style named catalogs (`workspaces.catalogs`).
    pub fn catalogs(&self) -> &BTreeMap<String, BTreeMap<String, String>> {
        static EMPTY: std::sync::OnceLock<BTreeMap<String, BTreeMap<String, String>>> =
            std::sync::OnceLock::new();
        match self {
            Workspaces::String(_) | Workspaces::Array(_) => EMPTY.get_or_init(BTreeMap::new),
            Workspaces::Object { catalogs, .. } => catalogs,
        }
    }
}

/// Process-wide cache of parsed `package.json` files keyed by absolute
/// path. Hit by `aube run` 2-3 times per invocation (prompt path, type
/// parse, External catch-all). Miss falls through to a fresh read +
/// parse and inserts into the cache.
static PACKAGE_JSON_CACHE: aube_util::cache::ProcessCache<PathBuf, PackageJson> =
    aube_util::cache::ProcessCache::new();

impl PackageJson {
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let _diag = aube_util::diag::Span::new(aube_util::diag::Category::Manifest, "from_path")
            .with_meta_fn(|| {
                // Emit only the file name to avoid leaking absolute paths
                // (and by extension home directory layout / employer-internal
                // build roots) into traces shared in bug reports.
                let display = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "package.json".to_string());
                format!(r#"{{"path":{}}}"#, aube_util::diag::jstr(&display))
            });
        let content =
            std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
        Self::parse(path, content)
    }

    /// Cached variant of [`Self::from_path`]. First caller per-path
    /// pays the read + parse; later callers receive an `Arc` clone.
    /// Errors are NOT cached (the next caller retries).
    pub fn from_path_cached(path: &Path) -> Result<std::sync::Arc<Self>, Error> {
        let key = path.to_path_buf();
        if let Some(hit) = PACKAGE_JSON_CACHE.get(&key) {
            return Ok(hit);
        }
        let parsed = Self::from_path(path)?;
        Ok(PACKAGE_JSON_CACHE.get_or_compute(key, || parsed))
    }

    /// Parse an in-memory `package.json` string. On failure, produces a
    /// [`Error::Parse`] with the source content and a span so `miette`'s
    /// `fancy` handler renders a pointer at the offending byte.
    pub fn parse(path: &Path, content: String) -> Result<Self, Error> {
        parse_json(path, content)
    }

    /// True when `peerDependenciesMeta.<name>.optional` is set.
    ///
    /// `peerDependenciesMeta` stays in the flattened `extra` map because
    /// most manifest consumers do not need it, but resolver and lockfile
    /// drift checks must agree on optional peer handling.
    pub fn peer_dependency_is_optional(&self, name: &str) -> bool {
        self.extra
            .get("peerDependenciesMeta")
            .and_then(|v| v.as_object())
            .and_then(|meta| meta.get(name))
            .and_then(|entry| entry.as_object())
            .and_then(|entry| entry.get("optional"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Iterate over non-optional `peerDependencies` entries.
    pub fn non_optional_peer_dependencies(&self) -> impl Iterator<Item = (&String, &String)> + '_ {
        self.peer_dependencies
            .iter()
            .filter(|(name, _)| !self.peer_dependency_is_optional(name))
    }

    /// Iterate over the `pnpm` and `aube` config objects in
    /// `package.json`, yielding whichever are present in precedence
    /// order (pnpm first, aube last). Callers that merge into a map
    /// with later-wins semantics get `aube.*` overriding `pnpm.*` on
    /// key conflict; callers that union lists get both sources
    /// included. Aube mirrors every `pnpm.*` config key under an
    /// `aube.*` alias so projects can declare aube-native config
    /// without piggy-backing on the pnpm namespace.
    fn pnpm_aube_objects(
        &self,
    ) -> impl Iterator<Item = &serde_json::Map<String, serde_json::Value>> {
        // Compatible namespaces first (lower precedence), then this tool's
        // own namespace (last = wins on key conflict). Standalone aube:
        // `["pnpm", "aube"]`. An empty `manifest_namespace` (root) carries
        // no object key and is skipped. The compatible (`pnpm`) namespaces are
        // gated on the engine context's `read_branded_pnpm_config` (upstream
        // default true) — an embedder under a non-pnpm incumbent clears it, so
        // only this tool's own namespace is consulted.
        let id = aube_util::embedder();
        let read_pnpm = aube_util::engine_context().read_branded_pnpm_config;
        let compatible = if read_pnpm { id.compatible_names } else { &[] };
        let self_ns = (!id.manifest_namespace.is_empty()).then_some(id.manifest_namespace);
        compatible
            .iter()
            .copied()
            .chain(self_ns)
            .filter_map(|k| self.extra.get(k).and_then(|v| v.as_object()))
    }

    /// Extract the `pnpm.allowBuilds` / `aube.allowBuilds` object from
    /// the raw `package.json` payload, if present. For embedders whose native
    /// manifest config lives at root (`manifest_namespace == ""`), the
    /// top-level `allowBuilds` map is *always* read — it is the embedder's own
    /// un-branded key (not a foreign brand's surface), so it is honored on
    /// every config surface (nub identity, npm/bun/yarn compat, pnpm/fresh),
    /// not only the root-native one. This is what makes `approve-builds`
    /// effective under an npm/bun/yarn incumbent: the approval it writes at the
    /// top level is read back here regardless of incumbent. Standalone aube
    /// keeps a non-empty `manifest_namespace`, so this branch never fires for
    /// it — its behavior is unchanged. Returns a map keyed by the raw
    /// pattern string (e.g. `"esbuild"`, `"@swc/core@1.3.0"`) with `bool`
    /// values preserved as `bool` and any other shape captured verbatim so the
    /// caller can warn about it. The tool's own namespace/root surface wins
    /// over compatible namespaces on key conflict.
    ///
    /// The key is held in `extra` rather than as a named field because
    /// it's nested under a `pnpm`/`aube` object.
    pub fn pnpm_allow_builds(&self) -> BTreeMap<String, AllowBuildRaw> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(map) = ns.get("allowBuilds").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    out.insert(k.clone(), AllowBuildRaw::from_json(v));
                }
            }
        }
        if aube_util::embedder().manifest_namespace.is_empty()
            && let Some(map) = self.extra.get("allowBuilds").and_then(|v| v.as_object())
        {
            for (k, v) in map {
                out.insert(k.clone(), AllowBuildRaw::from_json(v));
            }
        }
        out
    }

    /// Extract `pnpm.onlyBuiltDependencies` / `aube.onlyBuiltDependencies`
    /// as a flat list of package names allowed to run lifecycle
    /// scripts. This is pnpm's canonical allowlist key (used by nearly
    /// every real-world pnpm project) and coexists with `allowBuilds`
    /// — all sources merge into the same `BuildPolicy`. Non-string
    /// entries are dropped silently to match pnpm's tolerance for
    /// malformed configs. Entries from `aube.*` are appended after
    /// `pnpm.*` and deduped while preserving insertion order.
    pub fn pnpm_only_built_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns.get("onlyBuiltDependencies").and_then(|v| v.as_array()) {
                push_unique_strs(&mut out, arr);
            }
        }
        out
    }

    /// Extract `pnpm.neverBuiltDependencies` /
    /// `aube.neverBuiltDependencies` — the canonical denylist for
    /// lifecycle scripts. Entries override any allowlist match in
    /// `onlyBuiltDependencies` / `allowBuilds` since explicit denies
    /// always win in `BuildPolicy::decide`. Entries union across both
    /// namespaces with insertion order preserved.
    pub fn pnpm_never_built_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns.get("neverBuiltDependencies").and_then(|v| v.as_array()) {
                push_unique_strs(&mut out, arr);
            }
        }
        out
    }

    /// Extract the top-level `trustedDependencies` array — Bun's
    /// allowlist for lifecycle scripts. Treated as an additional
    /// allow-source alongside `pnpm.onlyBuiltDependencies`, so bun
    /// projects migrating to aube do not have to rewrite their manifest
    /// to get scripts running. Non-string entries are dropped; a denylist
    /// match in `neverBuiltDependencies` still wins at `decide()` time.
    pub fn trusted_dependencies(&self) -> Vec<String> {
        // Embedder seam: a tool whose active package manager ignores
        // `trustedDependencies` (everything but Bun, and Bun once it dropped
        // the field) flips this off in the engine context so the union
        // contributes nothing. Default on ⇒ upstream behavior.
        if !aube_util::engine_context().trusted_dependencies_honored {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(arr) = self
            .extra
            .get("trustedDependencies")
            .and_then(|v| v.as_array())
        {
            push_unique_strs(&mut out, arr);
        }
        out
    }

    /// pnpm-compatible `npm_package_*` environment pairs for lifecycle
    /// scripts. Mirrors what pnpm exports from a manifest: `name`,
    /// `version`, plus the deep-flattened `engines`, `config`, and
    /// `bin` fields (string `bin` → unsuffixed `npm_package_bin`).
    /// Other fields (`description`, `main`, `license`, `author`, …)
    /// are intentionally **not** exported — pnpm dropped whole-manifest
    /// flattening, and matching its exact allowlist keeps scripts that
    /// sniff these vars (`husky`, `node-pre-gyp`, …) behaving
    /// identically under aube.
    ///
    /// The `npm_package_json` path is set by the caller, which knows
    /// where the manifest lives on disk.
    ///
    /// Keys are "envified" the npm way: every character outside
    /// `[A-Za-z0-9_]` becomes `_`, so `config.my-key` →
    /// `npm_package_config_my_key`. Returned in a stable order
    /// (name, version, then engines/config/bin in key order) so the
    /// emitted environment is deterministic.
    pub fn npm_package_env(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(name) = &self.name {
            out.push(("npm_package_name".to_string(), name.clone()));
        }
        if let Some(version) = &self.version {
            out.push(("npm_package_version".to_string(), version.clone()));
        }
        for (key, value) in &self.engines {
            out.push((
                format!("npm_package_engines_{}", envify_env_key(key)),
                value.clone(),
            ));
        }
        if let Some(config) = self.extra.get("config") {
            flatten_json_env("npm_package_config", config, &mut out);
        }
        if let Some(bin) = self.extra.get("bin") {
            // Generic flatten, same as `config`: a string `bin` →
            // unsuffixed `npm_package_bin`, an object → `npm_package_bin_<key>`.
            // This mirrors pnpm exactly (verified against 11.5); npm instead
            // normalizes a string `bin` to the package's unscoped name first.
            flatten_json_env("npm_package_bin", bin, &mut out);
        }
        out
    }

    /// Extract `pnpm.catalog` / `aube.catalog` — a default catalog
    /// defined inline in package.json under the `pnpm`/`aube` object.
    /// pnpm itself reads catalogs only from `pnpm-workspace.yaml`, but
    /// aube also honors this location so single-package projects can
    /// declare catalogs without maintaining a separate workspace
    /// file. `aube.catalog` wins over `pnpm.catalog` on key conflict.
    pub fn pnpm_catalog(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(map) = ns.get("catalog").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        out
    }

    /// Extract `pnpm.catalogs` / `aube.catalogs` — named catalogs
    /// nested under the `pnpm`/`aube` object. Pairs with
    /// [`pnpm_catalog`] for a fully-package.json-local catalog
    /// declaration. Named catalogs merge per-key across namespaces
    /// (same rule as `pnpm_catalog`): `aube.catalogs.<name>.<pkg>`
    /// wins over `pnpm.catalogs.<name>.<pkg>`, while entries declared
    /// only on one side are preserved.
    pub fn pnpm_catalogs(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(outer) = ns.get("catalogs").and_then(|v| v.as_object()) {
                for (name, inner) in outer {
                    let Some(inner) = inner.as_object() else {
                        continue;
                    };
                    let catalog = out.entry(name.clone()).or_default();
                    for (k, v) in inner {
                        if let Some(s) = v.as_str() {
                            catalog.insert(k.clone(), s.to_string());
                        }
                    }
                }
            }
        }
        out
    }

    /// Extract `pnpm.ignoredOptionalDependencies` /
    /// `aube.ignoredOptionalDependencies` — a list of dep names that
    /// should be stripped from every manifest's `optionalDependencies`
    /// before resolution. Mirrors pnpm's read-package hook at
    /// `@pnpm/hooks.read-package-hook::createOptionalDependenciesRemover`.
    /// Non-string entries are ignored. Entries from both namespaces
    /// union into the returned set.
    pub fn pnpm_ignored_optional_dependencies(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns
                .get("ignoredOptionalDependencies")
                .and_then(|v| v.as_array())
            {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
        }
        out
    }

    /// Extract Bun's top-level `patchedDependencies` as a map of
    /// `name@version` -> patch file path (relative to the project root).
    /// Empty when the field is missing or malformed.
    pub fn bun_patched_dependencies(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(map) = self
            .extra
            .get("patchedDependencies")
            .and_then(|v| v.as_object())
        {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        out
    }

    /// Extract `pnpm.patchedDependencies` / `aube.patchedDependencies`
    /// as a map of `name@version` -> patch file path (relative to the
    /// project root). Empty when the field is missing or malformed.
    /// `aube.*` wins over `pnpm.*` on key conflict.
    pub fn pnpm_patched_dependencies(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(map) = ns.get("patchedDependencies").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        out
    }

    /// Return the set of dependency names marked
    /// `dependenciesMeta.<name>.injected = true`. When present, pnpm
    /// installs a hard copy of the resolved package (typically a
    /// workspace sibling) instead of a symlink, so the consumer sees
    /// the packed form — peer deps resolve against the consumer's
    /// tree rather than the source package's devDependencies. Aube's
    /// injection step reads this set after linking and rewrites each
    /// top-level symlink to point at a freshly materialized copy
    /// under `.aube/<name>@<version>+inject_<hash>/node_modules/<name>`.
    pub fn dependencies_meta_injected(&self) -> BTreeSet<String> {
        let Some(meta) = self
            .extra
            .get("dependenciesMeta")
            .and_then(|v| v.as_object())
        else {
            return BTreeSet::new();
        };
        meta.iter()
            .filter_map(|(k, v)| {
                let injected = v.get("injected").and_then(|b| b.as_bool()).unwrap_or(false);
                injected.then(|| k.clone())
            })
            .collect()
    }

    /// Return the set of dependency names marked
    /// `dependenciesMeta.<name>.built = false`. Yarn and pnpm read this
    /// as a per-package directive to skip that dependency's lifecycle
    /// (build) scripts — equivalent to a `neverBuiltDependencies` entry
    /// for the named package. Only `false` is a directive: `true` and
    /// absence both mean "build per the normal allow rules" (pnpm does
    /// not treat `built: true` as a force-allow), so this reader returns
    /// just the explicit `false` set, which the caller folds into the
    /// `BuildPolicy` denylist. `dependenciesMeta` is a neutral
    /// package.json field, so this applies for every incumbent.
    pub fn dependencies_meta_built_false(&self) -> BTreeSet<String> {
        let Some(meta) = self
            .extra
            .get("dependenciesMeta")
            .and_then(|v| v.as_object())
        else {
            return BTreeSet::new();
        };
        meta.iter()
            .filter_map(|(k, v)| {
                let built_false = v.get("built").and_then(|b| b.as_bool()) == Some(false);
                built_false.then(|| k.clone())
            })
            .collect()
    }

    /// Return `{pnpm,aube}.supportedArchitectures.{os,cpu,libc}` as
    /// three string arrays. Missing fields become empty vecs. Used by
    /// the resolver to widen the set of platforms considered
    /// installable for optional dependencies — e.g. resolving a
    /// lockfile for a different target than the host running `aube
    /// install`. Entries from `aube.*` are appended after `pnpm.*` and
    /// deduped while preserving insertion order.
    pub fn pnpm_supported_architectures(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut os = Vec::new();
        let mut cpu = Vec::new();
        let mut libc = Vec::new();
        for ns in self.pnpm_aube_objects() {
            let Some(sa) = ns.get("supportedArchitectures").and_then(|v| v.as_object()) else {
                continue;
            };
            if let Some(arr) = sa.get("os").and_then(|v| v.as_array()) {
                push_unique_strs(&mut os, arr);
            }
            if let Some(arr) = sa.get("cpu").and_then(|v| v.as_array()) {
                push_unique_strs(&mut cpu, arr);
            }
            if let Some(arr) = sa.get("libc").and_then(|v| v.as_array()) {
                push_unique_strs(&mut libc, arr);
            }
        }
        (os, cpu, libc)
    }

    /// Collect dependency overrides from every supported source on the
    /// root manifest, merged in precedence order: yarn-style
    /// `resolutions` (lowest), then `pnpm.overrides`, then
    /// `aube.overrides`, then top-level `overrides` (highest). Keys
    /// round-trip as their raw selector strings: bare name (`foo`),
    /// parent-chain (`parent>foo`), version-suffixed (`foo@<2`,
    /// `parent@1>foo`), and yarn wildcards (`**/foo`, `parent/foo`).
    /// Structural validation lives in `aube_resolver::override_rule`;
    /// this layer just filters out malformed keys and non-string
    /// values. Workspace-level overrides from `pnpm-workspace.yaml`
    /// are merged on top of this map by the caller.
    pub fn overrides_map(&self) -> BTreeMap<String, String> {
        // Embedder seam: when a tool has computed a scoped override source
        // (e.g. only the active PM's native field), use it verbatim and skip
        // the per-source walk below. Default `None` ⇒ upstream behavior.
        if let Some(scoped) = aube_util::engine_context().embedder_overrides {
            return scoped;
        }
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        let insert = |out: &mut BTreeMap<String, String>,
                      obj: &serde_json::Map<String, serde_json::Value>| {
            for (k, v) in obj {
                match v {
                    // Short-hand form: `"selector": "spec"`. The key is a
                    // raw pnpm/yarn selector that the resolver parses later.
                    serde_json::Value::String(s) if is_valid_selector_key(k) => {
                        out.insert(k.clone(), s.clone());
                    }
                    // npm's nested-object form: `"parent": { ".": "spec",
                    // "child": "spec" | { ... } }`. The nesting encodes an
                    // ancestor chain outermost-first, exactly mirroring
                    // pnpm's `parent>child` selector ordering, so we flatten
                    // it into selector keys rather than silently dropping the
                    // pin. (pnpm itself rejects object values; this branch
                    // only fires for npm-authored manifests.) See
                    // `flatten_nested_overrides` for the `.`/nesting rules.
                    serde_json::Value::Object(nested) => {
                        flatten_nested_overrides(k, nested, out);
                    }
                    _ => {}
                }
            }
        };

        // yarn `resolutions` (lowest priority)
        if let Some(obj) = self.extra.get("resolutions").and_then(|v| v.as_object()) {
            insert(&mut out, obj);
        }

        // `pnpm.overrides` then `aube.overrides` (later wins)
        for ns in self.pnpm_aube_objects() {
            if let Some(obj) = ns.get("overrides").and_then(|v| v.as_object()) {
                insert(&mut out, obj);
            }
        }

        // Top-level `overrides` (npm / pnpm) — highest priority
        if let Some(obj) = self.extra.get("overrides").and_then(|v| v.as_object()) {
            insert(&mut out, obj);
        }

        out
    }

    /// Look up a package name in `dependencies`, then `devDependencies`,
    /// then `optionalDependencies`, returning the declared version range.
    /// Mirrors the lookup order pnpm/npm use for `$name` override
    /// references. `peerDependencies` is intentionally excluded — a peer
    /// range isn't a dependency the root pins and reusing it as an
    /// override target would confuse rather than help.
    pub fn direct_dependency_range(&self, name: &str) -> Option<&str> {
        self.dependencies
            .get(name)
            .or_else(|| self.dev_dependencies.get(name))
            .or_else(|| self.optional_dependencies.get(name))
            .map(String::as_str)
    }

    /// Resolve `$name` override values in place against this manifest's
    /// direct dependencies, per pnpm/npm's documented sibling-reference
    /// syntax. Entries whose `$name` target isn't declared in
    /// `dependencies` / `devDependencies` / `optionalDependencies` are
    /// removed from the map; their raw selector keys are returned so the
    /// caller can surface a diagnostic. Non-`$` values pass through
    /// unchanged.
    pub fn resolve_override_refs(&self, overrides: &mut BTreeMap<String, String>) -> Vec<String> {
        let mut unresolved = Vec::new();
        overrides.retain(|key, value| {
            let Some(name) = value.strip_prefix('$') else {
                return true;
            };
            match self.direct_dependency_range(name) {
                Some(range) => {
                    *value = range.to_owned();
                    true
                }
                None => {
                    unresolved.push(key.clone());
                    false
                }
            }
        });
        unresolved
    }

    /// Extract `packageExtensions` from root package.json. Supports
    /// top-level `packageExtensions`, `pnpm.packageExtensions`, and
    /// `aube.packageExtensions`. Precedence (low → high):
    /// `pnpm.packageExtensions`, `aube.packageExtensions`, top-level
    /// `packageExtensions` — later writes win for duplicate selectors.
    pub fn package_extensions(&self) -> BTreeMap<String, serde_json::Value> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(obj) = ns.get("packageExtensions").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(obj) = self
            .extra
            .get("packageExtensions")
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Extract package deprecation mute ranges. Supports top-level
    /// `allowedDeprecatedVersions`, `pnpm.allowedDeprecatedVersions`,
    /// and `aube.allowedDeprecatedVersions`; later sources win for
    /// duplicate keys. Non-string values are ignored.
    pub fn allowed_deprecated_versions(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let insert = |out: &mut BTreeMap<String, String>,
                      obj: &serde_json::Map<String, serde_json::Value>| {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        };
        for ns in self.pnpm_aube_objects() {
            if let Some(obj) = ns
                .get("allowedDeprecatedVersions")
                .and_then(|v| v.as_object())
            {
                insert(&mut out, obj);
            }
        }
        if let Some(obj) = self
            .extra
            .get("allowedDeprecatedVersions")
            .and_then(|v| v.as_object())
        {
            insert(&mut out, obj);
        }
        out
    }

    /// Extract `{pnpm,aube}.peerDependencyRules.ignoreMissing` as a
    /// flat list of glob patterns. Non-string entries are dropped.
    /// Mirrors pnpm's `peerDependencyRules` escape hatch — patterns
    /// silence "missing required peer dependency" warnings when the
    /// peer name matches. Entries from both namespaces union in the
    /// returned list.
    pub fn pnpm_peer_dependency_rules_ignore_missing(&self) -> Vec<String> {
        self.pnpm_peer_dependency_rules_string_list("ignoreMissing")
    }

    /// Extract `{pnpm,aube}.peerDependencyRules.allowAny` as a flat
    /// list of glob patterns. Peers whose name matches a pattern have
    /// their semver check bypassed — any resolved version is accepted.
    pub fn pnpm_peer_dependency_rules_allow_any(&self) -> Vec<String> {
        self.pnpm_peer_dependency_rules_string_list("allowAny")
    }

    /// Extract `{pnpm,aube}.peerDependencyRules.allowedVersions` as a
    /// map of selector -> additional semver range. Selectors are
    /// either a bare peer name (e.g. `react`) meaning "applies to
    /// every consumer of this peer", or `parent>peer` (e.g.
    /// `styled-components>react`) meaning "only when declared by this
    /// parent". Values widen the declared peer range: a peer resolving
    /// inside *either* the declared range or this override is treated
    /// as satisfied. Non-string entries are ignored. `aube.*` wins
    /// over `pnpm.*` on key conflict.
    pub fn pnpm_peer_dependency_rules_allowed_versions(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            let Some(rules) = ns.get("peerDependencyRules").and_then(|v| v.as_object()) else {
                continue;
            };
            let Some(obj) = rules.get("allowedVersions").and_then(|v| v.as_object()) else {
                continue;
            };
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        out
    }

    fn pnpm_peer_dependency_rules_string_list(&self, field: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            let Some(rules) = ns.get("peerDependencyRules").and_then(|v| v.as_object()) else {
                continue;
            };
            let Some(arr) = rules.get(field).and_then(|v| v.as_array()) else {
                continue;
            };
            push_unique_strs(&mut out, arr);
        }
        out
    }

    /// Extract `updateConfig.ignoreDependencies` from package.json
    /// across all supported locations: top-level `updateConfig`,
    /// `pnpm.updateConfig.ignoreDependencies`, and
    /// `aube.updateConfig.ignoreDependencies`. All entries are merged
    /// and deduped.
    pub fn update_ignore_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns
                .get("updateConfig")
                .and_then(|v| v.as_object())
                .and_then(|u| u.get("ignoreDependencies"))
                .and_then(|v| v.as_array())
            {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
        }
        if let Some(update_config) = &self.update_config {
            out.extend(update_config.ignore_dependencies.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn all_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .iter()
            .chain(self.dev_dependencies.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn production_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Raw value shape for a single `allowBuilds` entry, preserved as-is
/// from the source JSON/YAML. Interpretation (allow / deny / warn
/// about unsupported shape) lives in `aube-scripts::policy`, keeping
/// this crate purely about parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowBuildRaw {
    Bool(bool),
    Other(String),
}

impl AllowBuildRaw {
    fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Bool(b) => Self::Bool(*b),
            // Strings are stored verbatim — without `as_str` we'd get
            // `Value::to_string`'s JSON-encoded form (`"foo"` with the
            // outer quotes baked into the payload), which would defeat
            // a downstream equality check against a known placeholder
            // and would also make any warning surface show extra
            // quotes the user didn't write.
            serde_json::Value::String(s) => Self::Other(s.clone()),
            other => Self::Other(other.to_string()),
        }
    }
}

/// Surface-level structural check on an override key. We accept any
/// non-empty key that isn't obviously a JSON typo — the resolver's
/// `override_rule` parser does the real work and silently drops keys
/// it can't interpret. Keeping the manifest filter loose means a pnpm
/// user with an unfamiliar-but-valid selector (e.g. `a@1>b@<2`)
/// reaches the resolver unchanged.
fn is_valid_selector_key(k: &str) -> bool {
    !k.is_empty()
}

/// Flatten one entry of npm's nested-object `overrides` form into pnpm
/// `>`-joined selector keys, inserting each resolved rule into `out`.
///
/// npm lets an override value be an object instead of a string:
///
/// ```json
/// "overrides": { "parent": { ".": "1.0.0", "child": "2.0.0" } }
/// ```
///
/// The nesting is an ancestor chain written outermost-first — the same
/// direction as pnpm's `parent>child` selector — so the translation is
/// exact:
///
/// - a string child `"child": "spec"` becomes `<chain>>child = spec`;
/// - the reserved key `"."` (npm's "this package itself") becomes a rule
///   targeting the current chain's innermost segment, i.e. selector
///   `<chain> = spec`;
/// - a nested object recurses with the child appended to the chain.
///
/// Outer keys may carry a version req (`"parent@2.0.0": { ... }`); they
/// pass through verbatim because aube's selector parser already accepts a
/// `name@req` segment. pnpm's own `overrides` are string-only (pnpm errors
/// on an object value), so this path is reached only for npm-authored
/// manifests, where silently dropping the pin would yield a wrong graph.
fn flatten_nested_overrides(
    chain: &str,
    nested: &serde_json::Map<String, serde_json::Value>,
    out: &mut BTreeMap<String, String>,
) {
    if chain.is_empty() {
        return;
    }
    for (key, val) in nested {
        match (key.as_str(), val) {
            (".", serde_json::Value::String(spec)) if is_valid_selector_key(chain) => {
                // Override the chain's own innermost package.
                out.insert(chain.to_string(), spec.clone());
            }
            (child, serde_json::Value::String(spec)) if child != "." => {
                let selector = format!("{chain}>{child}");
                if is_valid_selector_key(&selector) {
                    out.insert(selector, spec.clone());
                }
            }
            (child, serde_json::Value::Object(inner)) if child != "." => {
                let selector = format!("{chain}>{child}");
                flatten_nested_overrides(&selector, inner, out);
            }
            // `.` with a non-string value, or any other non-string leaf, is
            // not a valid npm override target — skip it.
            _ => {}
        }
    }
}

/// Append the string entries of `arr` to `dst`, skipping duplicates
/// already present and dropping non-string values. Preserves the
/// insertion order of first appearance — callers rely on this to keep
/// `pnpm.*` entries ahead of `aube.*` entries when both namespaces
/// contribute to the same list.
fn push_unique_strs(dst: &mut Vec<String>, arr: &[serde_json::Value]) {
    for v in arr {
        if let Some(s) = v.as_str()
            && !dst.iter().any(|existing| existing == s)
        {
            dst.push(s.to_string());
        }
    }
}

/// npm/pnpm "envify": replace every character that is not
/// `[A-Za-z0-9_]` with `_`, leaving case untouched. Used to build the
/// `npm_package_*` env-var keys from manifest field names.
fn envify_env_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Deep-flatten a JSON value into `prefix`-rooted `npm_package_*`
/// pairs, npm-style: objects recurse with `_`-joined keys, arrays
/// index with `_<i>`, scalars stringify, `null` is skipped.
fn flatten_json_env(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                flatten_json_env(&format!("{prefix}_{}", envify_env_key(k)), v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                flatten_json_env(&format!("{prefix}_{i}"), v, out);
            }
        }
        serde_json::Value::String(s) => out.push((prefix.to_string(), s.clone())),
        serde_json::Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        serde_json::Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
        serde_json::Value::Null => {}
    }
}

/// Union of `package.json`'s `{pnpm,aube}.supportedArchitectures.*` and
/// `pnpm-workspace.yaml`'s `supportedArchitectures.*`. pnpm v10 treats
/// the workspace yaml as the canonical home for shared platform
/// widening — a team generating a cross-platform lockfile on Linux CI
/// sets it there once rather than in every importer's manifest.
/// Insertion order: manifest first, workspace appended, duplicates
/// dropped (same dedupe rule `pnpm_supported_architectures` already
/// uses between the `pnpm.*` and `aube.*` namespaces).
pub fn effective_supported_architectures(
    manifest: &PackageJson,
    workspace: &workspace::WorkspaceConfig,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut os, mut cpu, mut libc) = manifest.pnpm_supported_architectures();
    if let Some(ws) = &workspace.supported_architectures {
        let extend_unique = |dst: &mut Vec<String>, src: &[String]| {
            for s in src {
                if !dst.iter().any(|existing| existing == s) {
                    dst.push(s.clone());
                }
            }
        };
        extend_unique(&mut os, &ws.os);
        extend_unique(&mut cpu, &ws.cpu);
        extend_unique(&mut libc, &ws.libc);
    }
    (os, cpu, libc)
}

/// Union of `package.json`'s `{pnpm,aube}.ignoredOptionalDependencies`
/// and `pnpm-workspace.yaml`'s `ignoredOptionalDependencies`. Same
/// layering rule as [`effective_supported_architectures`]: workspace
/// yaml is pnpm v10's canonical location for shared settings, so the
/// two sources union rather than override.
pub fn effective_ignored_optional_dependencies(
    manifest: &PackageJson,
    workspace: &workspace::WorkspaceConfig,
) -> BTreeSet<String> {
    let mut out = manifest.pnpm_ignored_optional_dependencies();
    out.extend(workspace.ignored_optional_dependencies.iter().cloned());
    out
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("failed to read {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Parse(Box<ParseError>),
    #[error("failed to parse {0}: {1}")]
    #[diagnostic(code(ERR_AUBE_MANIFEST_YAML_PARSE))]
    YamlParse(std::path::PathBuf, String),
}

/// JSON parse failure with enough info for `miette`'s `fancy` handler to
/// render a pointer at the offending byte. Boxed into [`Error::Parse`] so
/// the enum's `Err` size stays small (clippy's `result_large_err`).
///
/// `Diagnostic` is implemented by hand rather than via `miette::Diagnostic`
/// derive because `miette-derive` 7.6 expands into a destructuring that
/// triggers `unused_assignments` under `RUSTFLAGS=-D warnings` on rustc
/// 1.93 (our MSRV).
#[derive(Debug, thiserror::Error)]
#[error("failed to parse {path}: {message}")]
pub struct ParseError {
    pub path: std::path::PathBuf,
    pub message: String,
    pub src: miette::NamedSource<String>,
    pub span: miette::SourceSpan,
}

impl miette::Diagnostic for ParseError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new(aube_codes::errors::ERR_AUBE_MANIFEST_PARSE))
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        Some(&self.src)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        Some(Box::new(std::iter::once(
            miette::LabeledSpan::new_with_span(Some(self.message.clone()), self.span),
        )))
    }
}

impl ParseError {
    /// Build a `ParseError` from a `serde_json::Error`, computing the
    /// byte offset so miette can render a pointer into `content`.
    /// Shared across crates (see `aube_lockfile::Error::parse_json_err`)
    /// so there's a single implementation of the line/col → byte-offset
    /// conversion and span clamping.
    pub fn from_json_err(path: &Path, content: String, err: &serde_json::Error) -> Self {
        let offset = line_col_to_byte_offset(&content, err.line(), err.column());
        // Clamp the span length so it never extends past the content
        // end. A trailing-newline-EOF error reports a position at or
        // past `content.len()`; a fixed length of 1 would push the
        // range one byte past the source and miette's renderer would
        // fail to slice it. A zero-length span at `content.len()` is
        // what miette expects for "end-of-input" labels.
        let len = if offset >= content.len() { 0 } else { 1 };
        Self::new(path, content, err.to_string(), offset, len)
    }

    /// Build a `ParseError` from a `yaml_serde::Error`.
    /// `yaml_serde::Location::index` is already a byte offset, so no
    /// line/col conversion is needed. Errors without a location
    /// (notably those bubbling from `yaml_serde::from_value`) collapse
    /// to an empty span at offset 0 — miette still renders the file
    /// name + message, just without a pointer.
    pub fn from_yaml_err(path: &Path, content: String, err: &yaml_serde::Error) -> Self {
        let (offset, len) = match err.location() {
            Some(loc) => {
                let idx = loc.index().min(content.len());
                let len = if idx >= content.len() { 0 } else { 1 };
                (idx, len)
            }
            None => (0, 0),
        };
        Self::new(path, content, err.to_string(), offset, len)
    }

    fn new(path: &Path, content: String, message: String, offset: usize, len: usize) -> Self {
        ParseError {
            path: path.to_path_buf(),
            message,
            src: miette::NamedSource::new(path.display().to_string(), content),
            span: miette::SourceSpan::new(offset.into(), len),
        }
    }
}

/// Parse a JSON document from `content`, returning an [`Error::Parse`] on
/// failure with the source content + span attached so miette's fancy
/// handler can render a pointer into the offending file.
pub fn parse_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    content: String,
) -> Result<T, Error> {
    // Strip leading UTF-8 BOM (U+FEFF, bytes EF BB BF). Notepad on
    // Windows writes BOM by default. VS Code can be configured to do
    // the same. serde_json does not tolerate BOM, errors at "line 1
    // column 1". npm and pnpm both tolerate it. Without this strip,
    // opening package.json in Notepad, saving, then running aube
    // returns a cryptic parse error. Cheap fix, no downside.
    let content = if let Some(stripped) = content.strip_prefix('\u{FEFF}') {
        stripped.to_owned()
    } else {
        content
    };
    if let Ok(v) = sonic_rs::from_slice(content.as_bytes()) {
        return Ok(v);
    }
    match serde_json::from_str(&content) {
        Ok(v) => Ok(v),
        Err(e) => {
            let trimmed = content.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                return Err(Error::parse_msg(
                    path,
                    content,
                    "package.json cannot contain JSON comments. \
                     Remove any `//` or `/* */` lines. aube does not support JSONC for package.json"
                        .to_string(),
                ));
            }
            Err(Error::parse(path, content, &e))
        }
    }
}

/// Parse a YAML document from `content`, returning an [`Error::Parse`] on
/// failure with the source content + span attached. `yaml_serde` reports
/// errors with a `Location { index, line, column }` we can feed straight
/// into a miette span; type-mismatch errors raised after `from_str`
/// succeeds (e.g. via `from_value`) have no location and render without
/// a pointer but still carry the file name.
pub fn parse_yaml<T: serde::de::DeserializeOwned>(
    path: &Path,
    content: String,
) -> Result<T, Error> {
    match yaml_serde::from_str(&content) {
        Ok(v) => Ok(v),
        Err(e) => Err(Error::parse_yaml_err(path, content, &e)),
    }
}

impl Error {
    /// Build an [`Error::Parse`] from a `serde_json::Error`. Delegates
    /// to [`ParseError::from_json_err`] — the crate-shared constructor
    /// other crates (`aube-lockfile`) also reuse for their JSON parse
    /// paths.
    pub fn parse(path: &Path, content: String, err: &serde_json::Error) -> Self {
        Error::Parse(Box::new(ParseError::from_json_err(path, content, err)))
    }

    /// Build an [`Error::Parse`] from a `yaml_serde::Error`. Delegates
    /// to [`ParseError::from_yaml_err`].
    pub fn parse_yaml_err(path: &Path, content: String, err: &yaml_serde::Error) -> Self {
        Error::Parse(Box::new(ParseError::from_yaml_err(path, content, err)))
    }

    /// Build an [`Error::Parse`] with a plain message and a span
    /// pointing at the start of the file. Used for hand-crafted
    /// pre-parse diagnostics like the JSONC comment detector,
    /// where we want a clear message without serde's usual
    /// cryptic one.
    pub fn parse_msg(path: &Path, content: String, message: String) -> Self {
        let len = content.len();
        let src = miette::NamedSource::new(path.display().to_string(), content);
        let span = miette::SourceSpan::new(0.into(), len.min(1));
        Error::Parse(Box::new(ParseError {
            path: path.to_path_buf(),
            message,
            src,
            span,
        }))
    }
}

/// Convert serde_json's 1-based line/column into a byte offset into
/// `content`. Out-of-range values clamp to the end so we never panic
/// on a degenerate error position.
fn line_col_to_byte_offset(content: &str, line: usize, column: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut offset = 0usize;
    for (i, l) in content.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            return (offset + column.saturating_sub(1)).min(content.len());
        }
        offset += l.len();
    }
    content.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> PackageJson {
        serde_json::from_str(json).unwrap()
    }

    /// `npm_package_env` mirrors pnpm's exact flattening: name, version,
    /// and deep `engines`/`config`/`bin` — and nothing else.
    #[test]
    fn npm_package_env_matches_pnpm_allowlist() {
        let pkg = parse(
            r#"{
                "name": "@scope/envprobe",
                "version": "4.5.6",
                "description": "ignored",
                "main": "index.js",
                "type": "module",
                "private": true,
                "os": ["darwin"],
                "engines": { "node": ">=18", "npm": ">=9" },
                "config": { "port": "8080", "nested": { "deep-key": "x" } },
                "bin": { "probe": "./cli.js" }
            }"#,
        );
        let env: std::collections::BTreeMap<String, String> =
            pkg.npm_package_env().into_iter().collect();

        assert_eq!(
            env.get("npm_package_name").map(String::as_str),
            Some("@scope/envprobe")
        );
        assert_eq!(
            env.get("npm_package_version").map(String::as_str),
            Some("4.5.6")
        );
        assert_eq!(
            env.get("npm_package_engines_node").map(String::as_str),
            Some(">=18")
        );
        assert_eq!(
            env.get("npm_package_engines_npm").map(String::as_str),
            Some(">=9")
        );
        assert_eq!(
            env.get("npm_package_config_port").map(String::as_str),
            Some("8080")
        );
        // Nested objects flatten with `_`-joined, envified keys.
        assert_eq!(
            env.get("npm_package_config_nested_deep_key")
                .map(String::as_str),
            Some("x")
        );
        assert_eq!(
            env.get("npm_package_bin_probe").map(String::as_str),
            Some("./cli.js")
        );
        // Fields pnpm does not export must be absent.
        for absent in [
            "npm_package_description",
            "npm_package_main",
            "npm_package_type",
            "npm_package_private",
            "npm_package_os_0",
        ] {
            assert!(!env.contains_key(absent), "{absent} should not be exported");
        }
    }

    /// A bare-string `bin` flattens to the unsuffixed `npm_package_bin`
    /// (pnpm verified against 11.5) — the same generic flattening as
    /// `config`, not npm's normalize-to-unscoped-name behavior.
    #[test]
    fn npm_package_env_string_bin_is_unsuffixed() {
        let pkg = parse(r#"{ "name": "@scope/tool", "version": "1.0.0", "bin": "./cli.js" }"#);
        let env: std::collections::BTreeMap<String, String> =
            pkg.npm_package_env().into_iter().collect();
        assert_eq!(
            env.get("npm_package_bin").map(String::as_str),
            Some("./cli.js")
        );
        assert!(
            !env.keys().any(|k| k.starts_with("npm_package_bin_")),
            "string bin must stay unsuffixed: {env:?}"
        );
    }

    /// Pre-npm-2.x publishes (e.g. `extsprintf@1.4.1`, `coffee-script@1.3.3`)
    /// ship `"engines": ["node >=0.6.0"]` as an array rather than a map.
    /// Modern npm ignores the legacy shape; we do the same rather than
    /// fail the whole manifest and take down every install that touches
    /// one of these ancient packages.
    #[test]
    fn engines_legacy_array_form_parses_as_empty_map() {
        let p = parse(r#"{"name":"x","engines":["node >=0.6.0"]}"#);
        assert!(p.engines.is_empty());
    }

    /// Some old npm packument entries (e.g. `qs`) ship `engines` as a
    /// bare string. There is no reliable key to preserve, so treat it
    /// like the legacy array form and ignore it.
    #[test]
    fn engines_legacy_string_form_parses_as_empty_map() {
        let p = parse(r#"{"name":"x","engines":"node >=0.6.0"}"#);
        assert!(p.engines.is_empty());
    }

    #[test]
    fn engines_null_is_treated_as_empty() {
        let p = parse(r#"{"name":"x","engines":null}"#);
        assert!(p.engines.is_empty());
    }

    #[test]
    fn engines_modern_map_form_still_parses() {
        let p = parse(r#"{"name":"x","engines":{"node":">=18.0.0","npm":">=9"}}"#);
        assert_eq!(p.engines.get("node").unwrap(), ">=18.0.0");
        assert_eq!(p.engines.get("npm").unwrap(), ">=9");
    }

    #[test]
    fn engines_missing_field_is_empty() {
        let p = parse(r#"{"name":"x"}"#);
        assert!(p.engines.is_empty());
    }

    /// Sanity-check the line/column → byte-offset conversion. An
    /// off-by-one here silently slides miette's pointer to the wrong
    /// byte, defeating the whole reason the source span exists.
    #[test]
    fn line_col_offset_single_line_col_one() {
        assert_eq!(line_col_to_byte_offset("{}", 1, 1), 0);
    }

    #[test]
    fn line_col_offset_multiline_line_two() {
        // "a\nbc\n" — line 2, column 1 is the 'b' at byte 2.
        assert_eq!(line_col_to_byte_offset("a\nbc\n", 2, 1), 2);
        assert_eq!(line_col_to_byte_offset("a\nbc\n", 2, 2), 3);
    }

    /// `line == 0` can happen if serde_json hits EOF before any input;
    /// treat as "beginning of file" rather than panic.
    #[test]
    fn line_col_offset_line_zero_returns_zero() {
        assert_eq!(line_col_to_byte_offset("any", 0, 5), 0);
    }

    /// A column past the end of its line (or past EOF) clamps to the
    /// last valid offset so we never build a SourceSpan that would
    /// crash miette's renderer.
    #[test]
    fn line_col_offset_column_past_end_clamps() {
        let s = "ab";
        assert_eq!(line_col_to_byte_offset(s, 1, 999), s.len());
    }

    /// A line past the last line falls through the loop and clamps to
    /// the file end.
    #[test]
    fn line_col_offset_line_past_end_clamps() {
        let s = "a\nb";
        assert_eq!(line_col_to_byte_offset(s, 10, 1), s.len());
    }

    /// A file whose last line has no trailing `\n` is the common case;
    /// make sure columns on that final line still resolve correctly.
    #[test]
    fn line_col_offset_no_trailing_newline() {
        let s = "a\nbc";
        assert_eq!(line_col_to_byte_offset(s, 2, 2), 3);
    }

    /// `serde_json` reports "EOF while parsing" with a position at or
    /// past `content.len()` (e.g. `{"name":` → column 8 on a 8-byte
    /// buffer). The span must never extend past the end of source or
    /// `miette`'s renderer chokes trying to slice it — clamp the span
    /// length to 0 at EOF.
    #[test]
    fn parse_error_eof_span_stays_in_bounds() {
        let path = Path::new("pkg.json");
        let content = r#"{"name":"#.to_string();
        let json_err: serde_json::Error = serde_json::from_str::<serde_json::Value>(&content)
            .expect_err("truncated JSON must fail");
        let Error::Parse(pe) = Error::parse(path, content.clone(), &json_err) else {
            panic!("Error::parse must produce Parse variant");
        };
        let offset: usize = pe.span.offset();
        let len: usize = pe.span.len();
        assert!(
            offset + len <= content.len(),
            "span [{offset}, {}) exceeds content.len() {}",
            offset + len,
            content.len()
        );
    }

    /// A malformed YAML document should surface through `parse_yaml`
    /// as an `Error::Parse` carrying a `NamedSource` pointed at the
    /// supplied path and a span inside the content buffer.
    #[test]
    fn parse_yaml_attaches_source_span() {
        let path = Path::new("pnpm-workspace.yaml");
        // Tab as the first indent char is a spec-level YAML error;
        // yaml_serde reports a location for it.
        let content = "packages:\n\t- pkg\n".to_string();
        let res: Result<yaml_serde::Value, Error> = parse_yaml(path, content.clone());
        let Err(Error::Parse(pe)) = res else {
            panic!("parse_yaml must produce Parse variant on malformed input");
        };
        let offset: usize = pe.span.offset();
        let len: usize = pe.span.len();
        assert!(offset + len <= content.len());
        assert_eq!(pe.path, path);
    }

    /// `yaml_serde::from_value` errors have no `location()`. The helper
    /// should still produce an `Error::Parse` (with a zero-length span)
    /// so the file name survives into miette's render.
    #[test]
    fn parse_yaml_err_without_location_falls_back_to_empty_span() {
        let path = Path::new("pnpm-workspace.yaml");
        let content = String::new();
        let yaml_err: yaml_serde::Error =
            yaml_serde::from_value::<BTreeMap<String, String>>(yaml_serde::Value::Bool(true))
                .expect_err("bool cannot coerce to a map");
        assert!(yaml_err.location().is_none());
        let Error::Parse(pe) = Error::parse_yaml_err(path, content, &yaml_err) else {
            panic!("parse_yaml_err must produce Parse variant");
        };
        assert_eq!(pe.span.offset(), 0);
        assert_eq!(pe.span.len(), 0);
    }

    #[test]
    fn engines_map_drops_non_string_values() {
        // Stay consistent with how our dep-map parsers treat redacted
        // / non-string entries — drop, not fail.
        let p = parse(r#"{"name":"x","engines":{"node":">=18","weird":null,"n":42}}"#);
        assert_eq!(p.engines.get("node").unwrap(), ">=18");
        assert!(!p.engines.contains_key("weird"));
        assert!(!p.engines.contains_key("n"));
    }

    /// `firefox-profile@4.7.0` (and other legacy packages) ship tool
    /// config nested under `scripts`, e.g. `scripts.blanket = {...}`.
    /// npm ignores non-string entries instead of failing the install,
    /// and so do we — drop them and keep the real scripts.
    #[test]
    fn scripts_non_string_entries_are_dropped() {
        let p = parse(
            r#"{
                "name":"firefox-profile",
                "scripts": {
                    "test": "grunt travis",
                    "blanket": { "pattern": ["/lib/firefox_profile"] }
                }
            }"#,
        );
        assert_eq!(
            p.scripts.get("test").map(String::as_str),
            Some("grunt travis")
        );
        assert!(!p.scripts.contains_key("blanket"));
    }

    #[test]
    fn scripts_null_is_treated_as_empty() {
        let p = parse(r#"{"name":"x","scripts":null}"#);
        assert!(p.scripts.is_empty());
    }

    #[test]
    fn scripts_non_object_value_is_treated_as_empty() {
        // Mirrors `engines_tolerant`'s legacy-shape handling: if the
        // field exists but isn't a map, treat it as absent rather than
        // failing the parse.
        let p = parse(r#"{"name":"x","scripts":"oops"}"#);
        assert!(p.scripts.is_empty());
    }

    #[test]
    fn selector_key_filter_accepts_valid_forms() {
        assert!(is_valid_selector_key("lodash"));
        assert!(is_valid_selector_key("@babel/core"));
        assert!(is_valid_selector_key("foo>bar"));
        assert!(is_valid_selector_key("**/foo"));
        assert!(is_valid_selector_key("lodash@<4.17.21"));
        assert!(is_valid_selector_key("a@1>b@<2"));
    }

    #[test]
    fn selector_key_filter_rejects_empty() {
        assert!(!is_valid_selector_key(""));
    }

    #[test]
    fn overrides_map_collects_top_level() {
        let p = parse(r#"{"overrides": {"lodash": "4.17.21"}}"#);
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "4.17.21");
    }

    #[test]
    fn overrides_map_top_level_wins_over_pnpm_and_resolutions() {
        let p = parse(
            r#"{
                "resolutions": {"lodash": "1.0.0"},
                "pnpm": {"overrides": {"lodash": "2.0.0"}},
                "overrides": {"lodash": "3.0.0"}
            }"#,
        );
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "3.0.0");
    }

    #[test]
    fn overrides_map_merges_disjoint_keys() {
        let p = parse(
            r#"{
                "resolutions": {"a": "1"},
                "pnpm": {"overrides": {"b": "2"}},
                "overrides": {"c": "3"}
            }"#,
        );
        let m = p.overrides_map();
        assert_eq!(m.get("a").unwrap(), "1");
        assert_eq!(m.get("b").unwrap(), "2");
        assert_eq!(m.get("c").unwrap(), "3");
    }

    #[test]
    fn overrides_map_preserves_advanced_selector_keys() {
        // Advanced selectors round-trip as raw keys; the resolver
        // parses them later.
        let p = parse(
            r#"{
                "overrides": {
                    "lodash": "4.17.21",
                    "foo>bar": "1.0.0",
                    "**/baz": "1.0.0",
                    "qux@<2": "1.0.0"
                }
            }"#,
        );
        let m = p.overrides_map();
        assert_eq!(m.len(), 4);
        assert!(m.contains_key("lodash"));
        assert!(m.contains_key("foo>bar"));
        assert!(m.contains_key("**/baz"));
        assert!(m.contains_key("qux@<2"));
    }

    #[test]
    fn overrides_map_supports_npm_alias_value() {
        let p = parse(r#"{"overrides": {"foo": "npm:bar@^2"}}"#);
        assert_eq!(p.overrides_map().get("foo").unwrap(), "npm:bar@^2");
    }

    #[test]
    fn package_extensions_top_level_wins_over_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {"packageExtensions": {"foo": {"dependencies": {"a": "1"}}}},
                "packageExtensions": {"foo": {"dependencies": {"a": "2"}}}
            }"#,
        );
        assert_eq!(
            p.package_extensions()
                .get("foo")
                .and_then(|v| v.pointer("/dependencies/a"))
                .and_then(|v| v.as_str()),
            Some("2")
        );
    }

    #[test]
    fn update_ignore_dependencies_merges_top_level_and_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {"updateConfig": {"ignoreDependencies": ["a"]}},
                "updateConfig": {"ignoreDependencies": ["b"]}
            }"#,
        );
        assert_eq!(p.update_ignore_dependencies(), vec!["a", "b"]);
    }

    #[test]
    fn overrides_map_flattens_nested_child() {
        // npm's nested-object form: override `bar` only when it's a child
        // of `foo`. Flattens to the pnpm parent-chain selector `foo>bar`
        // instead of being silently dropped.
        let p = parse(r#"{"overrides": {"foo": {"bar": "1.0.0"}}}"#);
        let m = p.overrides_map();
        assert_eq!(m.get("foo>bar").map(String::as_str), Some("1.0.0"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn overrides_map_flattens_nested_dot_self_and_child() {
        // `.` overrides the parent itself; sibling keys override children
        // scoped under the parent. `foo` itself → 1.0.0; `bar` under foo
        // → 1.0.0.
        let p = parse(r#"{"overrides": {"foo": {".": "1.0.0", "bar": "2.0.0"}}}"#);
        let m = p.overrides_map();
        assert_eq!(m.get("foo").map(String::as_str), Some("1.0.0"));
        assert_eq!(m.get("foo>bar").map(String::as_str), Some("2.0.0"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn overrides_map_flattens_deeply_nested_chain() {
        // Arbitrary depth: override foo only when child of bar, only when
        // bar is a child of baz → selector `baz>bar>foo`.
        let p = parse(r#"{"overrides": {"baz": {"bar": {"foo": "1.0.0"}}}}"#);
        let m = p.overrides_map();
        assert_eq!(m.get("baz>bar>foo").map(String::as_str), Some("1.0.0"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn overrides_map_flattens_nested_with_versioned_parent() {
        // npm permits a version-pinned outer key: override foo only when
        // it's a child of bar@2.0.0. The `name@req` segment passes through
        // to the selector verbatim (the resolver parses it).
        let p = parse(r#"{"overrides": {"bar@2.0.0": {"foo": "1.0.0"}}}"#);
        let m = p.overrides_map();
        assert_eq!(m.get("bar@2.0.0>foo").map(String::as_str), Some("1.0.0"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn overrides_map_nested_does_not_drop_silently() {
        // Regression guard for the original bug: a manifest whose only
        // override is a nested object must NOT produce an empty map.
        let p = parse(r#"{"overrides": {"foo": {"bar": "1.0.0"}}}"#);
        assert!(
            !p.overrides_map().is_empty(),
            "nested-object overrides must not be silently dropped"
        );
    }

    #[test]
    fn overrides_map_nested_alias_value_round_trips() {
        // The replacement spec inside a nested form may be any value npm
        // accepts for a dependency, including an `npm:` alias.
        let p = parse(r#"{"overrides": {"foo": {"bar": "npm:baz@^2"}}}"#);
        assert_eq!(
            p.overrides_map().get("foo>bar").map(String::as_str),
            Some("npm:baz@^2")
        );
    }

    #[test]
    fn resolve_override_refs_substitutes_from_dependencies() {
        let p = parse(
            r#"{
                "dependencies": {"semver": "^7.5.2"},
                "overrides": {"semver@<7.5.2": "$semver"}
            }"#,
        );
        let mut m = p.overrides_map();
        let unresolved = p.resolve_override_refs(&mut m);
        assert!(unresolved.is_empty());
        assert_eq!(m.get("semver@<7.5.2").unwrap(), "^7.5.2");
    }

    #[test]
    fn resolve_override_refs_checks_dev_and_optional() {
        let p = parse(
            r#"{
                "devDependencies": {"a": "1.0.0"},
                "optionalDependencies": {"b": "2.0.0"},
                "overrides": {"a": "$a", "b": "$b"}
            }"#,
        );
        let mut m = p.overrides_map();
        let unresolved = p.resolve_override_refs(&mut m);
        assert!(unresolved.is_empty());
        assert_eq!(m.get("a").unwrap(), "1.0.0");
        assert_eq!(m.get("b").unwrap(), "2.0.0");
    }

    #[test]
    fn resolve_override_refs_drops_unresolved() {
        let p = parse(
            r#"{
                "dependencies": {"semver": "^7.5.2"},
                "overrides": {
                    "semver@<7.5.2": "$semver",
                    "cacheable-request@<10": "$cacheable-request"
                }
            }"#,
        );
        let mut m = p.overrides_map();
        let unresolved = p.resolve_override_refs(&mut m);
        assert_eq!(unresolved, vec!["cacheable-request@<10".to_string()]);
        assert_eq!(m.get("semver@<7.5.2").unwrap(), "^7.5.2");
        assert!(!m.contains_key("cacheable-request@<10"));
    }

    #[test]
    fn resolve_override_refs_passes_non_dollar_through() {
        let p = parse(
            r#"{
                "dependencies": {"foo": "1.0.0"},
                "overrides": {"foo": "2.0.0", "bar": "3.0.0"}
            }"#,
        );
        let mut m = p.overrides_map();
        let unresolved = p.resolve_override_refs(&mut m);
        assert!(unresolved.is_empty());
        assert_eq!(m.get("foo").unwrap(), "2.0.0");
        assert_eq!(m.get("bar").unwrap(), "3.0.0");
    }

    #[test]
    fn resolve_override_refs_ignores_peer_dependencies() {
        // npm/pnpm resolve `$name` against direct deps only. Peer
        // dependency ranges are contracts, not pins, so they shouldn't
        // silently flow into override values.
        let p = parse(
            r#"{
                "peerDependencies": {"react": "^18"},
                "overrides": {"react": "$react"}
            }"#,
        );
        let mut m = p.overrides_map();
        let unresolved = p.resolve_override_refs(&mut m);
        assert_eq!(unresolved, vec!["react".to_string()]);
        assert!(m.is_empty());
    }

    #[test]
    fn parses_bundled_dependencies_list() {
        let p = parse(r#"{"name":"x","bundledDependencies":["foo","bar"]}"#);
        let deps = BTreeMap::new();
        let names = p.bundled_dependencies.as_ref().unwrap().names(&deps);
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn accepts_legacy_bundle_dependencies_alias() {
        let p = parse(r#"{"name":"x","bundleDependencies":["foo"]}"#);
        assert!(matches!(
            p.bundled_dependencies,
            Some(BundledDependencies::List(_))
        ));
    }

    /// Regression: some publishes (e.g. `@lingui/message-utils@5.2.0`+)
    /// ship both `bundledDependencies` and the deprecated
    /// `bundleDependencies` alias in the same object. serde's default
    /// `alias` rejects that as a duplicate field; we accept it and
    /// prefer the canonical spelling.
    #[test]
    fn accepts_both_bundle_and_bundled_dependencies() {
        let p = parse(
            r#"{"name":"x","bundledDependencies":["canonical"],"bundleDependencies":["legacy"]}"#,
        );
        let deps = BTreeMap::new();
        let names = p.bundled_dependencies.as_ref().unwrap().names(&deps);
        assert_eq!(names, vec!["canonical"]);
    }

    /// Same regression with the keys in the other order, since serde
    /// field-collection is order-sensitive when alias collisions are
    /// involved.
    #[test]
    fn accepts_both_bundle_and_bundled_dependencies_reverse_order() {
        let p = parse(
            r#"{"name":"x","bundleDependencies":["legacy"],"bundledDependencies":["canonical"]}"#,
        );
        let deps = BTreeMap::new();
        let names = p.bundled_dependencies.as_ref().unwrap().names(&deps);
        assert_eq!(names, vec!["canonical"]);
    }

    #[test]
    fn bundle_true_means_all_production_deps() {
        let p =
            parse(r#"{"name":"x","dependencies":{"a":"1","b":"2"},"bundledDependencies":true}"#);
        let names = p
            .bundled_dependencies
            .as_ref()
            .unwrap()
            .names(&p.dependencies);
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn peer_dependency_rules_accessors_read_nested_pnpm_block() {
        let p = parse(
            r#"{
                "name":"x",
                "pnpm": {
                    "peerDependencyRules": {
                        "ignoreMissing": ["react", "react-dom"],
                        "allowAny": ["@types/*"],
                        "allowedVersions": {
                            "react": "^18.0.0",
                            "styled-components>react": "^17.0.0",
                            "ignored": 42
                        }
                    }
                }
            }"#,
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react".to_string(), "react-dom".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/*".to_string()],
        );
        let allowed = p.pnpm_peer_dependency_rules_allowed_versions();
        assert_eq!(allowed.get("react").map(String::as_str), Some("^18.0.0"));
        assert_eq!(
            allowed.get("styled-components>react").map(String::as_str),
            Some("^17.0.0"),
        );
        assert!(!allowed.contains_key("ignored"));
    }

    #[test]
    fn peer_dependency_rules_accessors_empty_when_missing() {
        let p = parse(r#"{"name":"x"}"#);
        assert!(p.pnpm_peer_dependency_rules_ignore_missing().is_empty());
        assert!(p.pnpm_peer_dependency_rules_allow_any().is_empty());
        assert!(p.pnpm_peer_dependency_rules_allowed_versions().is_empty());
    }

    // --- aube.* namespace parity --------------------------------------

    #[test]
    fn aube_namespace_read_when_pnpm_missing() {
        let p = parse(
            r#"{
                "aube": {
                    "onlyBuiltDependencies": ["esbuild"],
                    "neverBuiltDependencies": ["sharp"],
                    "ignoredOptionalDependencies": ["fsevents"],
                    "patchedDependencies": {"lodash@4.17.21": "patches/lodash.patch"},
                    "catalog": {"react": "^18.0.0"},
                    "catalogs": {"legacy": {"react": "^17.0.0"}},
                    "supportedArchitectures": {"os": ["linux", "win32"], "cpu": ["x64"]},
                    "overrides": {"lodash": "4.17.21"},
                    "packageExtensions": {"foo": {"dependencies": {"a": "1"}}},
                    "allowedDeprecatedVersions": {"request": "*"},
                    "peerDependencyRules": {
                        "ignoreMissing": ["react-native"],
                        "allowAny": ["@types/*"],
                        "allowedVersions": {"react": "^18.0.0"}
                    },
                    "updateConfig": {"ignoreDependencies": ["typescript"]},
                    "allowBuilds": {"esbuild": true}
                }
            }"#,
        );
        assert_eq!(p.pnpm_only_built_dependencies(), vec!["esbuild"]);
        assert_eq!(p.pnpm_never_built_dependencies(), vec!["sharp"]);
        assert!(p.pnpm_ignored_optional_dependencies().contains("fsevents"));
        assert_eq!(
            p.pnpm_patched_dependencies().get("lodash@4.17.21").unwrap(),
            "patches/lodash.patch",
        );
        assert_eq!(p.pnpm_catalog().get("react").unwrap(), "^18.0.0");
        assert_eq!(
            p.pnpm_catalogs()
                .get("legacy")
                .and_then(|c| c.get("react"))
                .unwrap(),
            "^17.0.0",
        );
        let (os, cpu, libc) = p.pnpm_supported_architectures();
        assert_eq!(os, vec!["linux", "win32"]);
        assert_eq!(cpu, vec!["x64"]);
        assert!(libc.is_empty());
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "4.17.21");
        assert!(p.package_extensions().contains_key("foo"));
        assert_eq!(p.allowed_deprecated_versions().get("request").unwrap(), "*",);
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react-native".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/*".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allowed_versions()
                .get("react")
                .unwrap(),
            "^18.0.0",
        );
        assert_eq!(p.update_ignore_dependencies(), vec!["typescript"]);
        assert!(matches!(
            p.pnpm_allow_builds().get("esbuild"),
            Some(AllowBuildRaw::Bool(true)),
        ));
    }

    #[test]
    fn dependencies_meta_built_false_collects_only_explicit_false() {
        // Yarn/pnpm: `built: false` = skip that dep's build scripts;
        // `built: true` and absence are the default (build normally) and
        // must NOT appear in the deny set.
        let p = parse(
            r#"{
                "name": "x",
                "dependenciesMeta": {
                    "esbuild": {"built": false},
                    "sharp": {"built": true},
                    "lodash": {"injected": true},
                    "node-gyp": {}
                }
            }"#,
        );
        let denied = p.dependencies_meta_built_false();
        assert!(denied.contains("esbuild"));
        assert!(!denied.contains("sharp"));
        assert!(!denied.contains("lodash"));
        assert!(!denied.contains("node-gyp"));
        assert_eq!(denied.len(), 1);
        // No `dependenciesMeta` at all → empty set.
        assert!(
            parse(r#"{"name":"y"}"#)
                .dependencies_meta_built_false()
                .is_empty()
        );
    }

    #[test]
    fn pnpm_allow_builds_round_trips_string_values_unwrapped() {
        // Regression: `serde_json::Value::to_string()` on a string value
        // produces JSON-encoded output (`"\"foo\""` with the outer
        // quotes baked in). `AllowBuildRaw::from_json` must store the
        // inner string verbatim so a downstream equality check against
        // a known placeholder works and any user-visible warning shows
        // the value the user actually wrote — not a re-quoted form.
        let p = parse(
            r#"{
                "pnpm": {
                    "allowBuilds": {
                        "esbuild": "set this to true or false"
                    }
                }
            }"#,
        );
        let map = p.pnpm_allow_builds();
        assert_eq!(
            map.get("esbuild"),
            Some(&AllowBuildRaw::Other(
                "set this to true or false".to_string()
            )),
        );
    }

    #[test]
    fn trusted_dependencies_reads_top_level_bun_format() {
        let p = parse(
            r#"{
                "trustedDependencies": ["esbuild", "sharp", "esbuild"]
            }"#,
        );
        assert_eq!(p.trusted_dependencies(), vec!["esbuild", "sharp"]);
    }

    #[test]
    fn trusted_dependencies_absent_returns_empty() {
        let p = parse(r#"{}"#);
        assert!(p.trusted_dependencies().is_empty());
    }

    #[test]
    fn trusted_dependencies_wrong_shape_returns_empty() {
        let p = parse(r#"{"trustedDependencies": {"esbuild": true}}"#);
        assert!(p.trusted_dependencies().is_empty());
    }

    #[test]
    fn bun_patched_dependencies_reads_top_level_field() {
        let p = parse(
            r#"{
                "patchedDependencies": {
                    "is-number@7.0.0": "patches/is-number.patch",
                    "ignored@1.0.0": false
                }
            }"#,
        );
        let got = p.bun_patched_dependencies();
        assert_eq!(
            got.get("is-number@7.0.0").unwrap(),
            "patches/is-number.patch"
        );
        assert!(!got.contains_key("ignored@1.0.0"));
    }

    #[test]
    fn aube_overrides_pnpm_on_key_conflict() {
        // For map-valued configs, `aube.*` wins on key conflict while
        // disjoint keys from either namespace merge.
        let p = parse(
            r#"{
                "pnpm": {
                    "catalog": {"react": "^17.0.0", "lodash": "^4.0.0"},
                    "patchedDependencies": {"foo@1": "pnpm.patch"},
                    "allowedDeprecatedVersions": {"request": "^2.0.0"},
                    "overrides": {"lodash": "pnpm-value"}
                },
                "aube": {
                    "catalog": {"react": "^18.0.0"},
                    "patchedDependencies": {"foo@1": "aube.patch"},
                    "allowedDeprecatedVersions": {"request": "^3.0.0"},
                    "overrides": {"lodash": "aube-value"}
                }
            }"#,
        );
        let catalog = p.pnpm_catalog();
        assert_eq!(catalog.get("react").unwrap(), "^18.0.0");
        assert_eq!(catalog.get("lodash").unwrap(), "^4.0.0");
        assert_eq!(
            p.pnpm_patched_dependencies().get("foo@1").unwrap(),
            "aube.patch",
        );
        assert_eq!(
            p.allowed_deprecated_versions().get("request").unwrap(),
            "^3.0.0",
        );
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "aube-value");
    }

    #[test]
    fn top_level_overrides_still_beat_aube_namespace() {
        // Top-level `overrides` is the npm-standard surface and
        // remains the highest-priority source.
        let p = parse(
            r#"{
                "pnpm": {"overrides": {"lodash": "1"}},
                "aube": {"overrides": {"lodash": "2"}},
                "overrides": {"lodash": "3"}
            }"#,
        );
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "3");
    }

    #[test]
    fn aube_supported_architectures_merges_with_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {"supportedArchitectures": {"os": ["linux"], "cpu": ["x64"]}},
                "aube": {"supportedArchitectures": {"os": ["win32"], "libc": ["glibc"]}}
            }"#,
        );
        let (os, cpu, libc) = p.pnpm_supported_architectures();
        assert_eq!(os, vec!["linux", "win32"]);
        assert_eq!(cpu, vec!["x64"]);
        assert_eq!(libc, vec!["glibc"]);
    }

    #[test]
    fn aube_list_configs_union_with_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {
                    "onlyBuiltDependencies": ["esbuild"],
                    "neverBuiltDependencies": ["sharp"],
                    "ignoredOptionalDependencies": ["fsevents"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react"],
                        "allowAny": ["@types/a"]
                    }
                },
                "aube": {
                    "onlyBuiltDependencies": ["swc"],
                    "neverBuiltDependencies": ["node-gyp"],
                    "ignoredOptionalDependencies": ["dtrace-provider"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react-native"],
                        "allowAny": ["@types/b"]
                    }
                }
            }"#,
        );
        assert_eq!(p.pnpm_only_built_dependencies(), vec!["esbuild", "swc"]);
        assert_eq!(p.pnpm_never_built_dependencies(), vec!["sharp", "node-gyp"]);
        let ignored = p.pnpm_ignored_optional_dependencies();
        assert!(ignored.contains("fsevents"));
        assert!(ignored.contains("dtrace-provider"));
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react".to_string(), "react-native".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/a".to_string(), "@types/b".to_string()],
        );
    }

    #[test]
    fn effective_supported_architectures_unions_manifest_and_workspace() {
        let p = parse(
            r#"{
                "pnpm": {
                    "supportedArchitectures": {
                        "os": ["current", "linux"],
                        "cpu": ["x64"]
                    }
                }
            }"#,
        );
        let ws: workspace::WorkspaceConfig = yaml_serde::from_str(
            r#"
supportedArchitectures:
  os: ["win32"]
  cpu: ["x64", "arm64"]
  libc: ["glibc"]
"#,
        )
        .unwrap();
        let (os, cpu, libc) = effective_supported_architectures(&p, &ws);
        // Manifest first, workspace appended, duplicates dropped.
        assert_eq!(os, vec!["current", "linux", "win32"]);
        assert_eq!(cpu, vec!["x64", "arm64"]);
        assert_eq!(libc, vec!["glibc"]);
    }

    #[test]
    fn effective_supported_architectures_works_without_either_source() {
        let p = parse(r#"{}"#);
        let ws = workspace::WorkspaceConfig::default();
        let (os, cpu, libc) = effective_supported_architectures(&p, &ws);
        assert!(os.is_empty() && cpu.is_empty() && libc.is_empty());
    }

    #[test]
    fn effective_ignored_optional_dependencies_unions_manifest_and_workspace() {
        let p = parse(
            r#"{
                "pnpm": { "ignoredOptionalDependencies": ["fsevents"] }
            }"#,
        );
        let ws: workspace::WorkspaceConfig = yaml_serde::from_str(
            r#"
ignoredOptionalDependencies:
  - dtrace-provider
  - fsevents
"#,
        )
        .unwrap();
        let merged = effective_ignored_optional_dependencies(&p, &ws);
        assert!(merged.contains("fsevents"));
        assert!(merged.contains("dtrace-provider"));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn aube_catalogs_merge_per_key_within_named_catalog() {
        // Same semantics as `pnpm_catalog`: aube wins per-key, and
        // entries only declared on one side are preserved instead of
        // being dropped when the catalog name exists on both sides.
        let p = parse(
            r#"{
                "pnpm": {
                    "catalogs": {
                        "default": {"react": "^17.0.0", "lodash": "^4.0.0"},
                        "legacy": {"webpack": "^4.0.0"}
                    }
                },
                "aube": {
                    "catalogs": {
                        "default": {"react": "^18.0.0", "vite": "^5.0.0"}
                    }
                }
            }"#,
        );
        let cats = p.pnpm_catalogs();
        let default = cats.get("default").expect("default catalog present");
        assert_eq!(default.get("react").unwrap(), "^18.0.0");
        assert_eq!(default.get("lodash").unwrap(), "^4.0.0");
        assert_eq!(default.get("vite").unwrap(), "^5.0.0");
        let legacy = cats.get("legacy").expect("legacy catalog preserved");
        assert_eq!(legacy.get("webpack").unwrap(), "^4.0.0");
    }

    #[test]
    fn aube_list_configs_dedupe_duplicates_across_namespaces() {
        // Union semantics imply dedup: a name listed in both
        // namespaces appears once, with first-seen ordering preserved.
        let p = parse(
            r#"{
                "pnpm": {
                    "onlyBuiltDependencies": ["esbuild", "sharp"],
                    "neverBuiltDependencies": ["evil"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react"],
                        "allowAny": ["@types/a"]
                    }
                },
                "aube": {
                    "onlyBuiltDependencies": ["esbuild", "swc"],
                    "neverBuiltDependencies": ["evil", "node-gyp"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react", "react-native"],
                        "allowAny": ["@types/a", "@types/b"]
                    }
                }
            }"#,
        );
        assert_eq!(
            p.pnpm_only_built_dependencies(),
            vec!["esbuild", "sharp", "swc"],
        );
        assert_eq!(p.pnpm_never_built_dependencies(), vec!["evil", "node-gyp"]);
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react".to_string(), "react-native".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/a".to_string(), "@types/b".to_string()],
        );
    }

    #[test]
    fn aube_update_config_merges_with_pnpm_and_top_level() {
        let p = parse(
            r#"{
                "pnpm": {"updateConfig": {"ignoreDependencies": ["a"]}},
                "aube": {"updateConfig": {"ignoreDependencies": ["b"]}},
                "updateConfig": {"ignoreDependencies": ["c"]}
            }"#,
        );
        assert_eq!(p.update_ignore_dependencies(), vec!["a", "b", "c"]);
    }
}
