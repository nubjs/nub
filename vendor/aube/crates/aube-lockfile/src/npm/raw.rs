use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct InstallPathInfo {
    pub(super) name: String,
    pub(super) dep_path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RawNpmLockfile {
    /// `Option` (not `u32`) because pre-2017 `npm-shrinkwrap.json` omits
    /// the field entirely — npm 3/4 wrote no `lockfileVersion`. The
    /// reader treats `None` as the legacy (nested-`dependencies`) format,
    /// the same as an explicit `lockfileVersion: 1`. A required `u32`
    /// here is what made an old shrinkwrap hard-fail at serde with
    /// `missing field lockfileVersion` before it could reach the
    /// version branch.
    #[serde(rename = "lockfileVersion", default)]
    pub(super) lockfile_version: Option<u32>,
    #[serde(default)]
    pub(super) packages: BTreeMap<String, RawNpmPackage>,
}

/// Pre-npm-7 lockfile shape: `package-lock.json` `lockfileVersion 1`
/// (npm 5/6) and pre-2017 `npm-shrinkwrap.json` (no `lockfileVersion`).
/// Both encode the resolution as a recursively-nested `dependencies`
/// tree instead of the flat install-path-keyed `packages` map the
/// v2/v3 reader consumes — `read::lift_legacy_to_packages` walks this
/// into that flat form so the rest of the reader is unchanged.
#[derive(Debug, Deserialize)]
pub(super) struct RawNpmLegacyLockfile {
    #[serde(default)]
    pub(super) dependencies: BTreeMap<String, RawNpmLegacyDep>,
}

/// One entry in the legacy nested `dependencies` tree. npm hoists the
/// shared version to the top level and nests only the conflicting one,
/// so nesting depth here is the package's install path
/// (`node_modules/<a>/node_modules/<b>/…`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawNpmLegacyDep {
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default)]
    pub(super) resolved: Option<String>,
    /// sha512 (npm 5.1+/6), sha1 (npm 5.0), or ABSENT (npm ≤4 / npm 3).
    /// aube's store verifies sha512, so a missing/sha1 hash is recovered
    /// at install time: a `None` integrity is filled from the streaming
    /// fetch's computed sha512 (`apply_computed_integrities`), and a
    /// sha1 is verified directly by the store (sha1 is in the SRI set
    /// `aube-store::integrity` accepts). Either way the install succeeds.
    #[serde(default)]
    pub(super) integrity: Option<String>,
    /// v1's single edge list (declared name → range). v1 predates the
    /// per-entry `optionalDependencies` split, so this lumps regular and
    /// optional deps; it maps onto `RawNpmPackage.dependencies`, which
    /// the reader uses to seed forward-refs and preserve declared ranges.
    #[serde(default)]
    pub(super) requires: BTreeMap<String, String>,
    /// npm marks an entry shipped inside a parent tarball's
    /// `bundleDependencies`. Maps to `in_bundle`; fidelity-only.
    #[serde(default)]
    pub(super) bundled: bool,
    /// Nested (non-hoisted) deps. Their install path is this entry's
    /// path plus `/node_modules/<name>`.
    #[serde(default)]
    pub(super) dependencies: BTreeMap<String, RawNpmLegacyDep>,
    // `from` (npm-internal spec string), `dev`, and `optional` are
    // intentionally not captured: `from` is recoverable from name+version,
    // and the v2/v3 pipeline classifies dev/optional from the root
    // manifest sections — not from per-entry flags — so capturing them
    // here would have no place to flow. See `lift_legacy_to_packages`.
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawNpmPackage {
    /// npm emits this field only when the entry is an npm-alias
    /// (`"h3-v2": "npm:h3@..."` resolves to `node_modules/h3-v2` with
    /// `name: "h3"`). For non-aliased packages the name is recoverable
    /// from the install path and npm omits the field. We use the
    /// presence of this field — combined with inequality against the
    /// install-path segment — to detect aliases.
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) version: Option<String>,
    #[serde(default)]
    pub(super) integrity: Option<String>,
    /// Full registry tarball URL npm wrote when it locked this entry.
    /// We capture it so aliased packages (whose registry name differs
    /// from the install-path-derived name used to key the graph) don't
    /// need to re-derive the URL from the registry base — and so we
    /// can round-trip `resolved:` faithfully when we write back.
    #[serde(default)]
    pub(super) resolved: Option<String>,
    #[serde(default)]
    pub(super) link: bool,
    /// Root importer only. npm mirrors the manifest's `workspaces` here,
    /// which is what lets the reader tell a member apart from a local
    /// directory dependency — the two are otherwise identical in shape.
    /// Read as the parsed enum so the array, bun object, and string forms
    /// all yield their patterns.
    #[serde(default)]
    pub(super) workspaces: Option<aube_manifest::Workspaces>,
    #[serde(default)]
    pub(super) dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) optional_dependencies: BTreeMap<String, String>,
    /// npm v7+ records `peerDependencies` verbatim on each package
    /// entry (pulled straight from the package's own `package.json`
    /// at lockfile-write time). The flat npm layout relies on peers
    /// being auto-installed into *some* ancestor `node_modules/` so
    /// Node's upward walk finds them, but aube's isolated layout
    /// wants them as explicit siblings — without this field, the
    /// resolver's peer-context pass has nothing to work with on the
    /// lockfile-driven install path and peers silently go missing
    /// from `.aube/<dep_path>/node_modules/`.
    #[serde(default)]
    pub(super) peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) peer_dependencies_meta: BTreeMap<String, RawNpmPeerDepMeta>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) libc: Vec<String>,
    /// Captured verbatim for round-trip. npm writes these on every
    /// package entry; dropping them on re-emit is one of the
    /// remaining sources of `aube install --no-frozen-lockfile`
    /// churn against native npm output.
    ///
    /// Uses `aube_manifest::engines_tolerant` so the legacy array
    /// shape (e.g. `ansi-html-community@0.0.8` ships
    /// `"engines": ["node >= 0.8.0"]` and npm preserves it verbatim
    /// in the lockfile) doesn't blow up the whole parse. We normalize
    /// the array to an empty map — same behavior modern npm gives the
    /// shape for engine-strict checks, and the same tolerance the
    /// manifest parser already applies.
    #[serde(default, deserialize_with = "aube_manifest::engines_tolerant")]
    pub(super) engines: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) bin: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) license: Option<RawNpmLicense>,
    #[serde(default)]
    pub(super) funding: Option<RawNpmFunding>,
    /// npm writes `hasInstallScript: true` on every package whose
    /// manifest declares an `install` / `preinstall` / `postinstall`
    /// script (or whose registry packument carried the flag). Captured
    /// verbatim so a parse → re-emit cycle doesn't drop it. npm only
    /// ever writes the field when `true`, so a missing key reads as
    /// `false` and the writer skips it — matching npm exactly.
    #[serde(default)]
    pub(super) has_install_script: bool,
    /// npm writes `hasShrinkwrap: true` on a package that ships its own
    /// `npm-shrinkwrap.json`. Rare in modern packages but part of npm's
    /// canonical per-package key set; preserved verbatim for round-trip.
    #[serde(default)]
    pub(super) has_shrinkwrap: bool,
    /// npm writes `inBundle: true` on a package that ships *inside*
    /// another package's tarball (its parent's `bundleDependencies`).
    /// npm records it so the installer doesn't try to fetch the entry
    /// from the registry. Preserved verbatim; the install path keys off
    /// the parent's `bundled_dependencies`, so this is round-trip
    /// fidelity only.
    #[serde(default)]
    pub(super) in_bundle: bool,
    /// npm copies the registry's deprecation message onto the locked
    /// entry (`deprecated: "<message>"`) so a later install can warn
    /// without re-hitting the registry. Verbatim round-trip.
    #[serde(default)]
    pub(super) deprecated: Option<String>,
    /// npm writes `bundleDependencies: ["name", …]` on a package that
    /// declares bundled deps. Already parsed at the manifest layer, but
    /// the lockfile reader needs to capture it too so a parse → re-emit
    /// cycle preserves the field on the package entry. npm tolerates the
    /// legacy `bundledDependencies` spelling on read; it always writes
    /// the `bundleDependencies` spelling, so we normalize to that on
    /// emit.
    #[serde(default, alias = "bundledDependencies")]
    pub(super) bundle_dependencies: Vec<String>,
}

/// npm's `license:` field on a package entry. Modern npm writes the
/// SPDX expression as a bare string, but older packages (e.g. `tv4`)
/// still ship the deprecated object / array-of-objects shapes that
/// npm copies verbatim from the package's `package.json`:
///
/// 1. SPDX string: `"license": "MIT"`
/// 2. object: `"license": {"type": "MIT", "url": "…"}`
/// 3. array: `"license": [{"type": "Public Domain", …}, {"type": "MIT", …}]`
///
/// Aube only carries a single `license: Option<String>` on
/// `LockedPackage`, so on read we collapse to the first usable
/// `type` (or bare string element); on write we always emit the
/// bare string form.
#[derive(Debug, Clone, Default)]
pub(super) struct RawNpmLicense {
    pub(super) value: Option<String>,
}

impl<'de> Deserialize<'de> for RawNpmLicense {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct LicenseVisitor;

        impl<'de> Visitor<'de> for LicenseVisitor {
            type Value = RawNpmLicense;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an SPDX string, a {type: ...} object, or an array of either")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmLicense {
                    value: Some(v.to_owned()),
                })
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmLicense { value: Some(v) })
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut value: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "type" {
                        value = map.next_value::<Option<String>>()?;
                    } else {
                        // Skip unknown fields (e.g. `url`).
                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(RawNpmLicense { value })
            }

            fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
            where
                S: SeqAccess<'de>,
            {
                // Pick the first usable license from the array; aube's
                // single-string model can't represent a list. Drain the
                // rest so the deserializer state stays consistent.
                let mut chosen: Option<String> = None;
                while let Some(item) = seq.next_element::<RawNpmLicense>()? {
                    if chosen.is_none() {
                        chosen = item.value;
                    }
                }
                Ok(RawNpmLicense { value: chosen })
            }
        }

        deserializer.deserialize_any(LicenseVisitor)
    }
}

/// npm's `funding:` block on a package entry. npm copies the field
/// verbatim from the package's `package.json`, which means all three
/// shapes the registry permits show up in real lockfiles:
///
/// 1. bare URL string: `"funding": "https://example.com/sponsor"`
/// 2. object: `"funding": {"url": "…", "type": "github"}`
/// 3. mixed array: `"funding": ["https://…", {"url": "…"}]`
///
/// Aube only carries a single `funding_url: Option<String>` on
/// `LockedPackage`, so on read we collapse to the first URL we find;
/// on write we always emit the single-key `{"url": …}` form (which
/// npm itself accepts on a re-read).
#[derive(Debug, Clone, Default)]
pub(super) struct RawNpmFunding {
    pub(super) url: Option<String>,
}

impl<'de> Deserialize<'de> for RawNpmFunding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct FundingVisitor;

        impl<'de> Visitor<'de> for FundingVisitor {
            type Value = RawNpmFunding;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a funding URL string, a {url: ...} object, or an array of either")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmFunding {
                    url: Some(v.to_owned()),
                })
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmFunding { url: Some(v) })
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut url: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "url" {
                        url = map.next_value::<Option<String>>()?;
                    } else {
                        // Skip unknown fields (e.g. `type`).
                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(RawNpmFunding { url })
            }

            fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
            where
                S: SeqAccess<'de>,
            {
                // Pick the first usable URL from the array; aube's
                // single-URL model can't represent a list. Drain the
                // rest so the deserializer state stays consistent.
                let mut chosen: Option<String> = None;
                while let Some(item) = seq.next_element::<RawNpmFunding>()? {
                    if chosen.is_none() {
                        chosen = item.url;
                    }
                }
                Ok(RawNpmFunding { url: chosen })
            }
        }

        deserializer.deserialize_any(FundingVisitor)
    }
}

/// `peerDependenciesMeta` value — only `optional` is meaningful to
/// us today (matches pnpm's model). Other fields that might appear
/// (`description`, etc.) are preserved only as far as serde's
/// `deny_unknown_fields` stays off.
#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct RawNpmPeerDepMeta {
    #[serde(default)]
    pub(super) optional: bool,
}
