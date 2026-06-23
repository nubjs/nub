use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// Visitor helper macros — each tolerant deserializer below picks the
// subset that matches its custom handlers. Splitting these granularly
// is what lets `funding_url` keep its own `visit_seq` (array case)
// while `FundingArrayEntry` keeps its own `visit_map` (object case),
// without colliding on duplicate method definitions.

/// Visit-primitives-as-default + `visit_some` re-entry. Covers the
/// shapes that have no structural content: `null`, bool, integer,
/// float. Every tolerant visitor in this file wants this.
macro_rules! visit_primitives_to {
    ($de:lifetime, $default:expr) => {
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_some<D2: Deserializer<$de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(self)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($default)
        }
    };
}

/// Drain a JSON array and return `$default`. Pulled into a macro because
/// the `Visitor::visit_seq` signature varies by `'de` lifetime and
/// `A: SeqAccess<'de>` bounds — same body, different traits.
macro_rules! visit_seq_to {
    ($de:lifetime, $default:expr) => {
        fn visit_seq<A: SeqAccess<$de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            while access.next_element::<IgnoredAny>()?.is_some() {}
            Ok($default)
        }
    };
}

/// Drain a JSON object and return `$default`.
macro_rules! visit_map_to {
    ($de:lifetime, $default:expr) => {
        fn visit_map<A: MapAccess<$de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            while access.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok($default)
        }
    };
}

/// Drop strings: `visit_str` / `visit_string` → `$default`. Used by the
/// `*_to_none_via_any` visitors that consider strings non-applicable
/// (e.g. `npm_user_tolerant`, which only accepts objects).
macro_rules! visit_strings_to {
    ($de:lifetime, $default:expr) => {
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
            Ok($default)
        }
    };
}

/// Deserialize a `BTreeMap<String, String>` tolerant to any non-string
/// value — both at the whole-map level (`"dist-tags": null` → empty
/// map) and at the value level (`{"latest": null}` or
/// `{"vows": {"version": "0.6.4", ...}}` → entry dropped).
///
/// Two real-world sources of non-string values:
///
/// 1. Registry proxies (notably JFrog Artifactory's npm remote) emit
///    `null` in places where npmjs.org always emits a string: stripped
///    / tombstoned `dist-tags` values, per-version `time` entries for
///    deleted versions, or dep-map entries that were redacted by a
///    mirroring filter.
/// 2. Ancient publishes — some packages from the 2012–2013 era
///    (`deep-diff@0.1.0`, for example) have `devDependencies` entries
///    shaped like `{"version": "0.6.4", "dependencies": {...}}`
///    instead of a plain version string, because an old npm client
///    serialized a resolved tree into the manifest.
///
/// A strict `BTreeMap<String, String>` shape would fail these with
/// `invalid type: ..., expected a string`, blocking an install of any
/// package whose packument merely *lists* an affected version — even
/// when the user's range doesn't select it. Drop the unparseable
/// entries so the resolver sees the same shape npmjs would have served
/// for a modern publish. pnpm and bun behave the same way.
///
/// Implemented as a direct serde `Visitor` rather than buffering
/// through `serde_json::Value` — the dep-object case (Value 2 above)
/// would otherwise allocate a nested `Map<String, Value>` per dropped
/// entry. The proptest suite in `tests/tolerant_deserializers.rs`
/// pins down parity with the old `Value`-based behavior.
fn non_string_tolerant_map<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;

    impl<'de> Visitor<'de> for V {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("null or an object mapping strings to strings")
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(BTreeMap::new())
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(BTreeMap::new())
        }

        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(self)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some(key) = access.next_key::<String>()? {
                let MaybeString(maybe) = access.next_value()?;
                if let Some(s) = maybe {
                    out.insert(key, s);
                }
            }
            Ok(out)
        }
    }

    de.deserialize_any(V)
}

pub mod client;
pub mod config;
pub mod jsr;
pub mod osv_bloom_client;
pub mod osv_mirror;
pub mod slow_metadata;
pub mod supply_chain;

// Packuments and `package.json` files share the `bundledDependencies`
// shape, so the registry crate borrows the type from `aube-manifest`
// rather than defining its own copy. Re-exported for resolver callers
// that already import this crate.
pub use aube_manifest::BundledDependencies;

/// Controls whether the registry client is allowed to hit the network.
///
/// Mirrors pnpm's `--offline` / `--prefer-offline`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkMode {
    /// Normal behavior: honor the packument TTL, revalidate with the
    /// registry when the cache is stale, fetch tarballs over the network.
    #[default]
    Online,
    /// Use the packument cache regardless of age; only hit the network on a
    /// cache miss. Tarballs fall back to the network when the store doesn't
    /// already have them.
    PreferOffline,
    /// Never hit the network. Packument and tarball fetches fail with
    /// `Error::Offline` if the requested data isn't already on disk.
    Offline,
}

/// A packument (package document) from the npm registry.
/// This is the metadata for all versions of a package.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Packument {
    pub name: String,
    #[serde(default)]
    pub modified: Option<String>,
    #[serde(default)]
    pub versions: BTreeMap<String, VersionMetadata>,
    #[serde(
        rename = "dist-tags",
        default,
        deserialize_with = "non_string_tolerant_map"
    )]
    pub dist_tags: BTreeMap<String, String>,
    /// Per-version publish timestamps (ISO-8601). Populated
    /// opportunistically: npmjs.org's corgi (abbreviated) packument
    /// omits `time`, but Verdaccio v5.15.1+ includes it in corgi, and
    /// the full-packument path used for `--resolution-mode=time-based`
    /// and `minimumReleaseAge` always carries it. When present, the
    /// resolver round-trips it into the lockfile's top-level `time:`
    /// block — matching pnpm's `publishedAt` wiring — and, in
    /// time-based mode, uses it to derive the publish-date cutoff.
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub time: BTreeMap<String, String>,
}

/// Metadata for a specific version of a package.
///
/// Deserializes via `VersionMetadataRaw` (`#[serde(from = ...)]`) so
/// that publishes carrying *both* `bundledDependencies` (canonical) and
/// `bundleDependencies` (deprecated alias) parse cleanly. serde's plain
/// `#[serde(alias = ...)]` rejects that as a duplicate field, which
/// blocks installs of every version of every package that ships both
/// keys (e.g. `@lingui/message-utils@>=5.2.0`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", from = "VersionMetadataRaw")]
pub struct VersionMetadata {
    pub name: String,
    pub version: String,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub peer_dependencies_meta: BTreeMap<String, PeerDepMeta>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub optional_dependencies: BTreeMap<String, String>,
    /// `bundledDependencies` from the packument. Either a list of dep
    /// names or `true` (meaning "bundle every `dependencies` entry").
    /// Packages listed here are shipped inside the parent tarball, so
    /// the resolver must not recurse into them. npm serializes this
    /// under both `bundledDependencies` and `bundleDependencies`; on
    /// deserialize we accept either, and prefer the canonical when both
    /// are present (handled in `VersionMetadataRaw`).
    pub bundled_dependencies: Option<BundledDependencies>,
    pub dist: Option<Dist>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub libc: Vec<String>,
    /// `engines:` from the package manifest (e.g. `{node: ">=8"}`).
    /// Round-tripped into the lockfile so pnpm-compatible output can
    /// emit `engines: {node: '>=8'}` on package entries without a
    /// packument re-fetch.
    ///
    /// Uses `aube_manifest::engines_tolerant` so the legacy pre-npm-2.x
    /// array shape (e.g. `madge@0.0.1` and `html-entities@1.x` ship
    /// `"engines": ["node >= 0.8.0"]`) doesn't blow up the whole
    /// packument — one such version would otherwise block install of
    /// any range that touches the packument, even when the user's
    /// selector doesn't pick that version. Array normalizes to an
    /// empty map, matching the manifest and lockfile parsers.
    #[serde(default, deserialize_with = "aube_manifest::engines_tolerant")]
    pub engines: BTreeMap<String, String>,
    /// `license:` field from the package manifest. npm's lockfile
    /// keeps this per-package; other formats don't. Stored as
    /// `Option<String>` because packuments can emit a bare string
    /// (`"MIT"`), an SPDX object, or nothing at all — we only keep
    /// the simple case for lockfile round-trip. Non-string shapes
    /// degrade to `None` rather than failing to parse the packument.
    #[serde(default, deserialize_with = "license_string")]
    pub license: Option<String>,
    /// `funding:` URL extracted from the manifest's `funding` field.
    /// The field is documented as a string *or* an object with a
    /// `url:` key *or* an array of either — npm's lockfile
    /// normalizes to `{url: …}`, so we only keep the URL and let
    /// the writer emit the wrapping object. Serde `rename` because
    /// `rename_all = "camelCase"` would otherwise look for
    /// `fundingUrl` in the JSON.
    #[serde(default, rename = "funding", deserialize_with = "funding_url")]
    pub funding_url: Option<String>,
    /// `bin:` map from the packument, normalized to `name → path`.
    ///
    /// npm records `bin` in two shapes on a manifest: a string
    /// (`"bin": "cli.js"` — implicitly named after the package) or a
    /// map (`"bin": {"foo": "cli.js"}` — explicitly named). We
    /// normalize to the map form at parse time so downstream callers
    /// don't have to branch: an empty map means "no bins".
    ///
    /// pnpm collapses this to `hasBin: true` on its package entries;
    /// bun preserves the full map on its per-package meta. Keeping
    /// the map lets us feed both writers without an extra
    /// tarball-level re-parse.
    #[serde(default, rename = "bin", deserialize_with = "bin_map")]
    pub bin: BTreeMap<String, String>,
    #[serde(default)]
    pub has_install_script: bool,
    /// Deprecation message from the registry, if this version is deprecated.
    #[serde(default, deserialize_with = "deprecated_string")]
    pub deprecated: Option<String>,
    /// npm staged-publish approval metadata. When present, the
    /// resolver treats it as the strongest trust evidence because the
    /// publish went through a registry-side approval flow.
    #[serde(default)]
    pub approver: Option<serde_json::Value>,
    /// `_npmUser` block from the packument, when present. The
    /// trust-policy check reads `_npmUser.trustedPublisher` as the
    /// strongest trust-evidence signal (npm's "trusted publishers"
    /// feature, OIDC-backed). Some old packuments emit `_npmUser` as
    /// a `"name <email>"` string rather than an object — that shape
    /// degrades to `None` instead of failing the whole packument.
    #[serde(default, rename = "_npmUser", deserialize_with = "npm_user_tolerant")]
    pub npm_user: Option<NpmUser>,
}

/// Deserialize-only mirror of [`VersionMetadata`] that splits the
/// `bundled_dependencies` field into two name-distinct slots so a
/// payload carrying *both* `bundledDependencies` and `bundleDependencies`
/// (e.g. `@lingui/message-utils@5.2.0`+) doesn't trip serde's duplicate
/// field check the way `#[serde(alias = ...)]` does. The canonical
/// spelling wins on merge — keeps parity with what npm renders for the
/// installed-tree view of the same package.
///
/// **Maintenance invariant:** every non-`bundled_dependencies` field
/// here must mirror its counterpart on [`VersionMetadata`] *byte-for-byte*
/// in serde attributes (`rename`, `deserialize_with`, `default`, etc.).
/// The `From` impl below catches missing fields at compile time, but
/// **attribute drift is silent** — e.g. dropping a `deserialize_with`
/// here makes the deserialize path strict on shapes the public type
/// silently tolerates. When adding or modifying a field on
/// `VersionMetadata`, update this struct in lockstep.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionMetadataRaw {
    name: String,
    version: String,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies_meta: BTreeMap<String, PeerDepMeta>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "bundledDependencies")]
    bundled_dependencies: Option<BundledDependencies>,
    #[serde(default, rename = "bundleDependencies")]
    bundle_dependencies_alias: Option<BundledDependencies>,
    dist: Option<Dist>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    libc: Vec<String>,
    #[serde(default, deserialize_with = "aube_manifest::engines_tolerant")]
    engines: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "license_string")]
    license: Option<String>,
    #[serde(default, rename = "funding", deserialize_with = "funding_url")]
    funding_url: Option<String>,
    #[serde(default, rename = "bin", deserialize_with = "bin_map")]
    bin: BTreeMap<String, String>,
    #[serde(default)]
    has_install_script: bool,
    #[serde(default, deserialize_with = "deprecated_string")]
    deprecated: Option<String>,
    #[serde(default)]
    approver: Option<serde_json::Value>,
    #[serde(default, rename = "_npmUser", deserialize_with = "npm_user_tolerant")]
    npm_user: Option<NpmUser>,
}

impl From<VersionMetadataRaw> for VersionMetadata {
    fn from(raw: VersionMetadataRaw) -> Self {
        Self {
            name: raw.name,
            version: raw.version,
            dependencies: raw.dependencies,
            dev_dependencies: raw.dev_dependencies,
            peer_dependencies: raw.peer_dependencies,
            peer_dependencies_meta: raw.peer_dependencies_meta,
            optional_dependencies: raw.optional_dependencies,
            bundled_dependencies: raw.bundled_dependencies.or(raw.bundle_dependencies_alias),
            dist: raw.dist,
            os: raw.os,
            cpu: raw.cpu,
            libc: raw.libc,
            engines: raw.engines,
            license: raw.license,
            funding_url: raw.funding_url,
            bin: raw.bin,
            has_install_script: raw.has_install_script,
            deprecated: raw.deprecated,
            approver: raw.approver,
            npm_user: raw.npm_user,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PeerDepMeta {
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct NpmUser {
    /// Structured npm trusted-publisher evidence for publishes that came
    /// through OIDC-backed automation (e.g. GitHub Actions). aube's
    /// trust-policy check requires an object with a non-empty `id`.
    #[serde(default, rename = "trustedPublisher")]
    pub trusted_publisher: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Dist {
    pub tarball: String,
    pub integrity: Option<String>,
    pub shasum: Option<String>,
    /// Unpacked tarball size in bytes (`dist.unpackedSize`). Present
    /// on most modern packuments, absent on older ones — used as the
    /// best-effort install-size estimate that the progress bar shows
    /// as `4.2 MB / ~13.8 MB`. Decimal MB to match every other PM.
    #[serde(default, rename = "unpackedSize")]
    pub unpacked_size: Option<u64>,
    /// Sigstore attestations block. The trust-policy check reads
    /// `dist.attestations.provenance` as rank-1 trust evidence when
    /// it is an object with an SLSA provenance `predicateType`. aube
    /// validates this metadata shape during install; it does not
    /// cryptographically verify the attached attestation bundle.
    #[serde(default)]
    pub attestations: Option<Attestations>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Attestations {
    #[serde(default)]
    pub provenance: Option<serde_json::Value>,
}

fn deprecated_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let MaybeString(maybe) = MaybeString::deserialize(de)?;
    Ok(maybe.filter(|s| !s.is_empty()))
}

/// Accept the packument's `license:` field in any of its documented
/// shapes (string, `{type, url}` object, or missing) and collapse to
/// the simple string form npm emits in its lockfile. Non-string
/// shapes degrade to `None`; we don't try to normalize SPDX
/// expressions or license-file references here.
fn license_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a license string, a {type, url} object, or null")
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok((!s.is_empty()).then(|| s.to_owned()))
        }

        fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Self::Value, E> {
            Ok((!s.is_empty()).then_some(s))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut found: Option<String> = None;
            while let Some(key) = access.next_key::<String>()? {
                if key == "type" && found.is_none() {
                    let MaybeString(maybe) = access.next_value()?;
                    found = maybe.filter(|s| !s.is_empty());
                } else {
                    let _: IgnoredAny = access.next_value()?;
                }
            }
            Ok(found)
        }

        visit_primitives_to!('de, None);
        visit_seq_to!('de, None);
    }
    de.deserialize_any(V)
}

/// Extract the first `url:` out of a packument's `funding:` field.
/// The field may be a URL string, a `{url: …}` object, or an array
/// of either — npm's lockfile normalizes to `{"url": "…"}` on each
/// package entry, so we only need the URL itself. Missing / empty
/// / non-url-bearing shapes degrade to `None`.
fn funding_url<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a funding URL string, a {url} object, or an array of either")
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok((!s.is_empty()).then(|| s.to_owned()))
        }

        fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Self::Value, E> {
            Ok((!s.is_empty()).then_some(s))
        }

        fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
            extract_url_from_map(access)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            // Walk the array via `FundingArrayEntry`, which performs the
            // same string-or-{url} extraction per element. First non-empty
            // hit wins — drain the rest with `IgnoredAny` so we don't
            // re-parse elements we'll discard.
            while let Some(FundingArrayEntry(maybe)) = access.next_element()? {
                if let Some(s) = maybe {
                    while access.next_element::<IgnoredAny>()?.is_some() {}
                    return Ok(Some(s));
                }
            }
            Ok(None)
        }

        visit_primitives_to!('de, None);
    }
    de.deserialize_any(V)
}

/// Shared map-walk for `funding`'s top-level object form and per-array-element
/// object form: extract a non-empty `url` field, ignore everything else.
fn extract_url_from_map<'de, A: MapAccess<'de>>(mut access: A) -> Result<Option<String>, A::Error> {
    let mut found: Option<String> = None;
    while let Some(key) = access.next_key::<String>()? {
        if key == "url" && found.is_none() {
            let MaybeString(maybe) = access.next_value()?;
            found = maybe.filter(|s| !s.is_empty());
        } else {
            let _: IgnoredAny = access.next_value()?;
        }
    }
    Ok(found)
}

/// Accept the packument's `_npmUser:` field in its documented shapes
/// and degrade to `None` for anything else. Modern packuments emit an
/// object (`{name, email, trustedPublisher?}`); pre-2010 publishes
/// emit `"name <email>"` strings. We only care about
/// `trustedPublisher`, so unparseable shapes don't fail the packument
/// — they just lose the trusted-publisher signal for that version.
fn npm_user_tolerant<'de, D>(de: D) -> Result<Option<NpmUser>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<NpmUser>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an _npmUser object or any other JSON value")
        }

        // Object case: walk the map manually and pluck `trustedPublisher`
        // — the only `NpmUser` field anyone reads (see `aube-resolver`'s
        // trust-policy check). Deferring to `NpmUser::deserialize` on the
        // live `MapAccess` would propagate any deserialize error up and
        // fail the whole packument parse, which silently breaks the
        // tolerant contract the moment `NpmUser` gains a non-defaulted
        // field. New fields that need reading from the packument get
        // their extraction added here.
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut trusted_publisher: Option<serde_json::Value> = None;
            while let Some(key) = access.next_key::<String>()? {
                if key == "trustedPublisher" && trusted_publisher.is_none() {
                    trusted_publisher = access.next_value::<Option<serde_json::Value>>()?;
                } else {
                    let _: IgnoredAny = access.next_value()?;
                }
            }
            Ok(Some(NpmUser { trusted_publisher }))
        }

        visit_primitives_to!('de, None);
        visit_seq_to!('de, None);
        visit_strings_to!('de, None);
    }
    de.deserialize_any(V)
}

/// Normalize `package.json` `bin` into a `name → path` map.
///
/// Two canonical shapes on the npm registry: a string
/// (`"bin": "cli.js"` — implicitly keyed by the package name) and a
/// map (`"bin": {"foo": "cli.js"}`). Older or odd packuments also
/// surface `null` or an empty string; a missing `bin` field falls
/// through to the default empty map.
///
/// The string-form needs the package name to emit a well-formed map
/// — which we don't have here at deserialize time. We leave the key
/// as an empty string; every call site that cares about bin names
/// (`aube-linker`'s bin-symlink pass, the bun writer) already has
/// the package name in scope and can patch it up.
fn bin_map<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a bin string, a bin map, or null")
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
            if s.is_empty() {
                return Ok(BTreeMap::new());
            }
            let mut m = BTreeMap::new();
            m.insert(String::new(), s.to_owned());
            Ok(m)
        }

        fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Self::Value, E> {
            if s.is_empty() {
                return Ok(BTreeMap::new());
            }
            let mut m = BTreeMap::new();
            m.insert(String::new(), s);
            Ok(m)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some(key) = access.next_key::<String>()? {
                let MaybeString(maybe) = access.next_value()?;
                if let Some(s) = maybe {
                    out.insert(key, s);
                }
            }
            Ok(out)
        }

        visit_primitives_to!('de, BTreeMap::new());
        visit_seq_to!('de, BTreeMap::new());
    }
    de.deserialize_any(V)
}

/// Map value adapter: returns `Some(String)` for any JSON string, `None`
/// for everything else. Drains seqs/maps without allocation so non-string
/// values (including deeply nested dep-objects from ancient publishes)
/// never get materialized as a `serde_json::Value`.
struct MaybeString(Option<String>);

impl<'de> Deserialize<'de> for MaybeString {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MaybeString;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<MaybeString, E> {
                Ok(MaybeString(Some(s.to_owned())))
            }

            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<MaybeString, E> {
                Ok(MaybeString(Some(s)))
            }

            visit_primitives_to!('de, MaybeString(None));
            visit_seq_to!('de, MaybeString(None));
            visit_map_to!('de, MaybeString(None));
        }
        de.deserialize_any(V)
    }
}

/// Array-element adapter for `funding`'s array case: yields `Some(url)`
/// for a non-empty string or an object element with a non-empty `url`
/// field; `None` otherwise. Mirrors the top-level `funding_url` shape.
struct FundingArrayEntry(Option<String>);

impl<'de> Deserialize<'de> for FundingArrayEntry {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FundingArrayEntry;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a funding URL string or a {url} object")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<FundingArrayEntry, E> {
                Ok(FundingArrayEntry((!s.is_empty()).then(|| s.to_owned())))
            }

            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<FundingArrayEntry, E> {
                Ok(FundingArrayEntry((!s.is_empty()).then_some(s)))
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                access: A,
            ) -> Result<FundingArrayEntry, A::Error> {
                Ok(FundingArrayEntry(extract_url_from_map(access)?))
            }

            visit_primitives_to!('de, FundingArrayEntry(None));
            visit_seq_to!('de, FundingArrayEntry(None));
        }
        de.deserialize_any(V)
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("package not found: {0}")]
    #[diagnostic(code(ERR_AUBE_PACKAGE_NOT_FOUND))]
    NotFound(String),
    #[error("version not found: {0}@{1}")]
    #[diagnostic(code(ERR_AUBE_VERSION_NOT_FOUND))]
    VersionNotFound(String, String),
    /// The registry rejected the request with 401 — no auth token was
    /// configured, or the one we sent was invalid/expired. Callers
    /// should point the user at `aube login`. A 403 is a *different*
    /// case (authenticated but forbidden) and maps to [`Error::Forbidden`].
    #[error("authentication required")]
    #[diagnostic(code(ERR_AUBE_UNAUTHORIZED))]
    Unauthorized,
    /// The registry rejected the request with 403 — the request was
    /// authenticated but the credentials lack permission for this
    /// operation. The cause is almost always actionable and registry-
    /// specific (insufficient token scope, package blocked by an
    /// org/Artifactory policy, write to a read-only mirror), and
    /// registries put that detail in the response body — so we preserve
    /// it here verbatim. `aube login` does NOT fix a 403, so callers must
    /// surface the body and a permissions-oriented hint, not an auth one.
    /// The body may be empty if the registry sent none.
    #[error("forbidden by registry: {body}")]
    #[diagnostic(code(ERR_AUBE_FORBIDDEN))]
    Forbidden { body: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry rejected write: HTTP {status}: {body}")]
    #[diagnostic(code(ERR_AUBE_REGISTRY_WRITE_REJECTED))]
    RegistryWrite { status: u16, body: String },
    #[error("offline: {0} is not available in the local cache")]
    #[diagnostic(code(ERR_AUBE_OFFLINE))]
    Offline(String),
    /// The caller passed a package name that does not match the npm
    /// name grammar. Returned eagerly (before any I/O) so a hostile
    /// packument or manifest cannot use the cache-path builder as an
    /// arbitrary-file-write primitive.
    #[error("invalid package name: {0:?}")]
    #[diagnostic(code(ERR_AUBE_INVALID_PACKAGE_NAME))]
    InvalidName(String),
}

impl Error {
    /// True when the error represents an upstream backpressure
    /// signal worth feeding into [`aube_util::adaptive::AdaptiveLimit::record_throttle`].
    /// HTTP 429 / 502 / 503 / 504 and request timeouts qualify.
    /// Plain 4xx (NotFound, Unauthorized, ValidationError) and IO
    /// errors don't — shrinking the concurrency cap won't help
    /// those, and would over-react to transient hostile-input
    /// failures (a typo'd package name shouldn't halve the limit).
    pub fn is_throttle(&self) -> bool {
        match self {
            Error::Http(e) => {
                if e.is_timeout() {
                    return true;
                }
                matches!(
                    e.status().map(|s| s.as_u16()),
                    Some(429) | Some(502) | Some(503) | Some(504)
                )
            }
            Error::RegistryWrite { status, .. } => matches!(*status, 429 | 502 | 503 | 504),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> VersionMetadata {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn libc_accepts_string() {
        let v = parse(r#"{"name":"x","version":"1.0.0","libc":"glibc"}"#);
        assert_eq!(v.libc, vec!["glibc"]);
    }

    #[test]
    fn libc_accepts_array() {
        let v = parse(r#"{"name":"x","version":"1.0.0","libc":["glibc","musl"]}"#);
        assert_eq!(v.libc, vec!["glibc", "musl"]);
    }

    #[test]
    fn os_and_cpu_accept_string() {
        let v = parse(r#"{"name":"x","version":"1.0.0","os":"linux","cpu":"x64"}"#);
        assert_eq!(v.os, vec!["linux"]);
        assert_eq!(v.cpu, vec!["x64"]);
    }

    #[test]
    fn null_is_treated_as_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0","os":null,"cpu":null,"libc":null}"#);
        assert!(v.os.is_empty());
        assert!(v.cpu.is_empty());
        assert!(v.libc.is_empty());
    }

    /// Napi-rs emits `"libc": [null]` on Windows/macOS native-binding
    /// publishes (e.g. `@oxc-parser/binding-win32-x64-msvc`), meaning
    /// "no libc constraint". Drop the null entry so the packument
    /// parses — otherwise every version with that shape blocks resolve.
    #[test]
    fn libc_array_containing_null_drops_null() {
        let v = parse(r#"{"name":"x","version":"1.0.0","libc":[null]}"#);
        assert!(v.libc.is_empty());
    }

    #[test]
    fn os_cpu_libc_arrays_drop_non_string_entries() {
        let v = parse(
            r#"{
                "name":"x","version":"1.0.0",
                "os":["linux",null,42],
                "cpu":["x64",null],
                "libc":["glibc",{"x":1}]
            }"#,
        );
        assert_eq!(v.os, vec!["linux"]);
        assert_eq!(v.cpu, vec!["x64"]);
        assert_eq!(v.libc, vec!["glibc"]);
    }

    #[test]
    fn approver_metadata_is_extracted() {
        let v = parse(
            r#"{
                "name":"x",
                "version":"1.0.0",
                "approver":{"name":"release-manager"}
            }"#,
        );
        assert_eq!(
            v.approver
                .as_ref()
                .and_then(|a| a.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("release-manager")
        );
    }

    #[test]
    fn bin_normalizes_packument_shapes() {
        let missing = parse(r#"{"name":"x","version":"1.0.0"}"#);
        assert!(missing.bin.is_empty(), "missing bin → empty map");
        let empty_string = parse(r#"{"name":"x","version":"1.0.0","bin":""}"#);
        assert!(empty_string.bin.is_empty(), "empty string bin → empty map");
        let null_bin = parse(r#"{"name":"x","version":"1.0.0","bin":null}"#);
        assert!(null_bin.bin.is_empty(), "null bin → empty map");
        let empty_map = parse(r#"{"name":"x","version":"1.0.0","bin":{}}"#);
        assert!(empty_map.bin.is_empty(), "empty map bin → empty map");
        // String bin leaves the name blank — callers patch it with the
        // package name before materializing a symlink / writing to
        // bun.lock.
        let string_bin = parse(r#"{"name":"x","version":"1.0.0","bin":"cli.js"}"#);
        assert_eq!(string_bin.bin.get(""), Some(&"cli.js".to_string()));
        let map_bin = parse(r#"{"name":"x","version":"1.0.0","bin":{"foo":"cli.js"}}"#);
        assert_eq!(map_bin.bin.get("foo"), Some(&"cli.js".to_string()));
    }

    /// Round-trip the `bin` map through the on-disk cache format
    /// (serialize → parse). Regression: the disk cache round-trips
    /// the field under the name `bin`, so the deserializer *must*
    /// accept a map back (and not interpret the already-normalized
    /// map as an implicit-name string).
    #[test]
    fn bin_map_roundtrips_through_cache_serialization() {
        let mut bin = BTreeMap::new();
        bin.insert("semver".to_string(), "bin/semver.js".to_string());
        let v = VersionMetadata {
            name: "semver".to_string(),
            version: "7.7.4".to_string(),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            bundled_dependencies: None,
            dist: None,
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            engines: BTreeMap::new(),
            license: None,
            funding_url: None,
            bin,
            has_install_script: false,
            deprecated: None,
            approver: None,
            npm_user: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: VersionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.bin.get("semver"),
            Some(&"bin/semver.js".to_string()),
            "bin map must round-trip through cache serialization"
        );
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0"}"#);
        assert!(v.os.is_empty());
        assert!(v.cpu.is_empty());
        assert!(v.libc.is_empty());
    }

    #[test]
    fn attestations_provenance_is_extracted() {
        let v = parse(
            r#"{"name":"x","version":"1.0.0",
                "dist":{"tarball":"t","attestations":{"provenance":{"predicateType":"slsa"}}}}"#,
        );
        let dist = v.dist.expect("dist present");
        let att = dist.attestations.expect("attestations present");
        assert!(att.provenance.is_some(), "provenance present");
    }

    #[test]
    fn attestations_missing_is_none() {
        let v = parse(r#"{"name":"x","version":"1.0.0","dist":{"tarball":"t"}}"#);
        let dist = v.dist.expect("dist present");
        assert!(dist.attestations.is_none());
    }

    #[test]
    fn npm_user_object_with_trusted_publisher_is_parsed() {
        let v = parse(
            r#"{"name":"x","version":"1.0.0",
                "_npmUser":{"name":"u","email":"u@x","trustedPublisher":{"id":"gh"}}}"#,
        );
        let user = v.npm_user.expect("_npmUser present");
        assert!(user.trusted_publisher.is_some());
    }

    #[test]
    fn npm_user_object_without_trusted_publisher_is_parsed() {
        let v = parse(r#"{"name":"x","version":"1.0.0","_npmUser":{"name":"u","email":"u@x"}}"#);
        let user = v.npm_user.expect("_npmUser present");
        assert!(user.trusted_publisher.is_none());
    }

    /// Regression guard: the tolerant deserializer extracts
    /// `trustedPublisher` *manually* and ignores everything else, so
    /// payloads containing arbitrary garbage in the other `_npmUser`
    /// fields don't fail the whole packument parse. Pinning this means
    /// future contributors can't accidentally route through
    /// `NpmUser::deserialize` again — which would propagate any new
    /// non-defaulted-field error up and break tolerance for real-world
    /// packuments that pre-date the new field.
    #[test]
    fn npm_user_garbage_sibling_fields_still_extract_trusted_publisher() {
        let v = parse(
            r#"{"name":"x","version":"1.0.0",
                "_npmUser":{
                    "trustedPublisher":{"id":"gh"},
                    "name":42,
                    "email":[null,{"deep":{"nested":"garbage"}}],
                    "future_field_with_strict_type":"this would fail strict deserialize"
                }}"#,
        );
        let user = v
            .npm_user
            .expect("_npmUser present despite garbage siblings");
        assert!(user.trusted_publisher.is_some());
    }

    /// Pre-2010 publishes serialize `_npmUser` as a `"name <email>"`
    /// string. Degrade to `None` instead of failing the whole packument.
    #[test]
    fn npm_user_string_form_degrades_to_none() {
        let v = parse(r#"{"name":"x","version":"1.0.0","_npmUser":"isaacs <i@npmjs.com>"}"#);
        assert!(v.npm_user.is_none());
    }

    #[test]
    fn npm_user_null_or_missing_is_none() {
        let v_null = parse(r#"{"name":"x","version":"1.0.0","_npmUser":null}"#);
        assert!(v_null.npm_user.is_none());
        let v_missing = parse(r#"{"name":"x","version":"1.0.0"}"#);
        assert!(v_missing.npm_user.is_none());
    }

    #[test]
    fn deprecated_string_is_preserved_and_false_is_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0","deprecated":"use y"}"#);
        assert_eq!(v.deprecated.as_deref(), Some("use y"));

        let v = parse(r#"{"name":"x","version":"1.0.1","deprecated":false}"#);
        assert!(v.deprecated.is_none());
    }

    /// Artifactory's npm remote proxies sometimes emit `null` entries
    /// in dep maps where stripped/redacted deps used to be. The
    /// resolver must not bail on that — the null dep is semantically
    /// "not present", same shape npmjs would have served.
    #[test]
    fn dependency_maps_drop_null_entries() {
        let v = parse(
            r#"{
                "name": "x",
                "version": "1.0.0",
                "dependencies": {"kept": "^1", "stripped": null},
                "devDependencies": {"dkept": "^2", "dstripped": null},
                "peerDependencies": {"pkept": "^3", "pstripped": null},
                "optionalDependencies": {"okept": "^4", "ostripped": null}
            }"#,
        );
        assert_eq!(v.dependencies.len(), 1);
        assert_eq!(v.dependencies["kept"], "^1");
        assert_eq!(v.dev_dependencies.len(), 1);
        assert_eq!(v.peer_dependencies.len(), 1);
        assert_eq!(v.optional_dependencies.len(), 1);
    }

    /// Ancient publishes (e.g. `deep-diff@0.1.0`, published 2013) have
    /// dep-map entries where the value is an object
    /// (`{"version": "0.6.4", "dependencies": {...}}`) rather than a
    /// version string. That shape would fail a strict string-valued
    /// map — drop those entries, same as null ones, so the packument
    /// still parses and unaffected versions stay resolvable.
    #[test]
    fn dependency_maps_drop_object_valued_entries() {
        let v = parse(
            r#"{
                "name": "deep-diff",
                "version": "0.1.0",
                "devDependencies": {
                    "vows": {"version": "0.6.4", "dependencies": {"diff": {"version": "1.0.4"}}},
                    "extend": {"version": "1.1.1"},
                    "lodash": "0.9.2"
                }
            }"#,
        );
        assert_eq!(v.dev_dependencies.len(), 1);
        assert_eq!(v.dev_dependencies["lodash"], "0.9.2");
    }

    #[test]
    fn dependency_maps_null_whole_field_is_empty() {
        let v = parse(
            r#"{
                "name": "x",
                "version": "1.0.0",
                "dependencies": null,
                "devDependencies": null,
                "peerDependencies": null,
                "optionalDependencies": null
            }"#,
        );
        assert!(v.dependencies.is_empty());
        assert!(v.dev_dependencies.is_empty());
        assert!(v.peer_dependencies.is_empty());
        assert!(v.optional_dependencies.is_empty());
    }

    fn parse_packument(json: &str) -> Packument {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn packument_dist_tags_drops_null_tag() {
        let p = parse_packument(
            r#"{
                "name": "pkg",
                "dist-tags": {"latest": "1.2.3", "beta": null}
            }"#,
        );
        assert_eq!(p.dist_tags.len(), 1);
        assert_eq!(p.dist_tags["latest"], "1.2.3");
    }

    #[test]
    fn packument_dist_tags_null_whole_field_is_empty() {
        let p = parse_packument(r#"{"name":"pkg","dist-tags":null}"#);
        assert!(p.dist_tags.is_empty());
    }

    #[test]
    fn packument_preserves_modified_timestamp() {
        let p = parse_packument(
            r#"{
                "name": "pkg",
                "modified": "2026-04-14T14:26:11.557Z"
            }"#,
        );
        assert_eq!(p.modified.as_deref(), Some("2026-04-14T14:26:11.557Z"));
    }

    #[test]
    fn packument_time_drops_null_entries() {
        let p = parse_packument(
            r#"{
                "name": "pkg",
                "time": {"1.0.0": "2024-01-01T00:00:00.000Z", "0.9.0": null}
            }"#,
        );
        assert_eq!(p.time.len(), 1);
        assert!(p.time.contains_key("1.0.0"));
    }

    #[test]
    fn packument_time_null_whole_field_is_empty() {
        let p = parse_packument(r#"{"name":"pkg","time":null}"#);
        assert!(p.time.is_empty());
    }

    /// Pre-npm-2.x publishes (e.g. `madge@0.0.1`, `html-entities@1.x`)
    /// ship `"engines": ["node >= 0.8.0"]` as an array, and some old
    /// entries (e.g. `qs`) ship a bare string. npmjs.org serves those
    /// shapes verbatim in packuments. A strict map-only deserializer
    /// fails the whole packument parse, blocking install of any range
    /// that even lists an affected version. Normalize legacy non-map
    /// forms to an empty map — same tolerance the manifest and
    /// lockfile parsers already apply.
    #[test]
    fn engines_accepts_legacy_array_shape() {
        let v = parse(r#"{"name":"madge","version":"0.0.1","engines":["node >= 0.8.0"]}"#);
        assert!(v.engines.is_empty());
    }

    #[test]
    fn engines_accepts_legacy_string_shape() {
        let v = parse(r#"{"name":"qs","version":"0.6.0","engines":"node >= 0.4.0"}"#);
        assert!(v.engines.is_empty());
    }

    #[test]
    fn engines_accepts_map_shape() {
        let v = parse(r#"{"name":"x","version":"1.0.0","engines":{"node":">=18"}}"#);
        assert_eq!(v.engines.get("node"), Some(&">=18".to_string()));
    }

    #[test]
    fn engines_null_is_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0","engines":null}"#);
        assert!(v.engines.is_empty());
    }

    /// Regression: `@lingui/message-utils@5.2.0`+ ships the full
    /// packument with both `bundledDependencies` (canonical) and
    /// `bundleDependencies` (deprecated alias) carrying the same value.
    /// serde's `#[serde(alias)]` rejects that as a duplicate field,
    /// which used to fail the packument parse and abort install.
    #[test]
    fn bundled_deps_accepts_both_canonical_and_alias() {
        let v = parse(
            r#"{
                "name":"x","version":"1.0.0",
                "bundledDependencies":["canonical"],
                "bundleDependencies":["legacy"]
            }"#,
        );
        let deps = BTreeMap::new();
        let names = v.bundled_dependencies.as_ref().unwrap().names(&deps);
        assert_eq!(names, vec!["canonical"]);
    }

    #[test]
    fn bundled_deps_falls_back_to_alias_only() {
        let v = parse(r#"{"name":"x","version":"1.0.0","bundleDependencies":["legacy"]}"#);
        let deps = BTreeMap::new();
        let names = v.bundled_dependencies.as_ref().unwrap().names(&deps);
        assert_eq!(names, vec!["legacy"]);
    }
}
