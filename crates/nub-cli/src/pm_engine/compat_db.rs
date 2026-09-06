//! The bundled `packageExtensions` compatibility database.
//!
//! A package whose published code imports something its own manifest never
//! declares resolves under npm's flat `node_modules` — the upward directory walk
//! finds a copy something else installed — and fails under an isolated layout.
//! `packageExtensions` repairs the manifest at resolve time, and both pnpm and
//! Yarn ship a curated database of such repairs.
//!
//! nub ships a LARGER one. The vendored engine already carries Yarn's 158
//! entries plus pnpm's 3 additions, byte-faithful to upstream; this module adds
//! the machine-derived database published as `@nubjs/extensions`, produced by
//! running nub's own phantom detector over the 10,000 most-downloaded packages.
//! It is a strict superset — a gate in that project fails the build if any Yarn
//! rule is weakened — so the two layers agree wherever they overlap.
//!
//! **This diverges from pnpm deliberately.** For 25 packages the extra rules
//! carry a hard `dependencies` edge, so `nub install` materializes a package
//! `pnpm install` does not; the remaining ~629 are optional peers, which install
//! nothing on their own and instead repair resolution under a strict layout
//! (Yarn PnP with no fallback, pnpm with hoisting off). `ignoreCompatibilityDb`
//! declines the whole thing, both layers together.
//!
//! # Why a snapshot rather than a fetch
//!
//! `nubjs/package-extensions` rebuilds daily and publishes whenever the rules
//! move. Tracking that live would make resolution depend on a registry round
//! trip and let the same nub build install different trees on different days.
//! Vendoring pins one dataset per nub release and makes each refresh a
//! reviewable commit — `node scripts/sync-package-extensions.mjs`.
//!
//! Refreshing is safe for existing projects: this feeds
//! `EngineContext::bundled_package_extensions`, which is read only when
//! resolving a package and never by the lockfile `packageExtensionsChecksum`,
//! so a bump cannot drift a lockfile or abort a frozen install.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The vendored database, verbatim. Keys are sorted in the file so a refresh
/// diffs as the rules that changed rather than as the scan's iteration order.
const BUNDLED: &str = include_str!("../../assets/bundled-package-extensions.json");

/// The bundled `selector -> extension body` map, parsed once.
///
/// Parsed once, on first use, rather than eagerly: the map is only ever built
/// on a path that registers the engine context, so a command that never touches
/// the package manager does not pay for it.
pub(crate) fn bundled_package_extensions() -> &'static BTreeMap<String, serde_json::Value> {
    static PARSED: OnceLock<BTreeMap<String, serde_json::Value>> = OnceLock::new();
    PARSED.get_or_init(|| {
        // The asset is vendored and gated by the test below, so a parse failure
        // is a build-time mistake rather than anything a user can provoke.
        // Degrade to no database rather than aborting an install over it: the
        // engine's own Yarn/pnpm catalogs still apply, and a missing repair
        // surfaces as the phantom it was hiding.
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(BUNDLED) else {
            tracing::warn!("bundled packageExtensions database is not valid JSON; skipping it");
            return BTreeMap::new();
        };
        doc.get("packageExtensions")
            .and_then(|v| v.as_object())
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The asset parses, is non-trivial, and every body is an object keyed only
    /// by the four fields both package managers apply.
    ///
    /// A malformed asset does not fail the build — the loader degrades to an
    /// empty map on purpose — so without this the database could silently go
    /// missing and every install would just quietly stop being repaired.
    #[test]
    fn the_bundled_database_loads_and_is_well_formed() {
        let db = bundled_package_extensions();
        assert!(
            db.len() > 500,
            "the bundled database should carry the published dataset, got {} entries",
            db.len()
        );
        for (selector, body) in db {
            let obj = body
                .as_object()
                .unwrap_or_else(|| panic!("{selector}: extension body is not an object"));
            assert!(!obj.is_empty(), "{selector}: empty extension body");
            for field in obj.keys() {
                assert!(
                    matches!(
                        field.as_str(),
                        "dependencies"
                            | "optionalDependencies"
                            | "peerDependencies"
                            | "peerDependenciesMeta"
                    ),
                    "{selector}: unknown field `{field}` — the engine applies only the four \
                     manifest fields, so anything else is silently ignored"
                );
            }
        }
    }

    /// The database is a superset of the engine's vendored Yarn catalog, which
    /// is the property that lets nub layer it underneath without weakening a
    /// hand-curated rule. Spot-checked on the entry that motivated the layer.
    #[test]
    fn the_bundled_database_carries_the_curated_rules_it_extends() {
        let db = bundled_package_extensions();
        let reactcss = db
            .get("reactcss@*")
            .expect("the published dataset carries every Yarn rule, including reactcss@*");
        assert_eq!(
            reactcss
                .pointer("/peerDependencies/react")
                .and_then(|v| v.as_str()),
            Some("*"),
            "reactcss must keep the required react peer Yarn curated by hand"
        );
        // A rule Yarn does not have, and the reason for shipping the larger set:
        // `@datadog/sketches-js` declares protobufjs only in devDependencies,
        // while `dist/ddsketch/proto/compiled.js` requires `protobufjs/minimal`.
        let sketches = db
            .get("@datadog/sketches-js@*")
            .expect("the published dataset carries rules Yarn's does not");
        assert!(
            sketches.pointer("/dependencies/protobufjs").is_some(),
            "@datadog/sketches-js must gain protobufjs as a real dependency"
        );
    }
}
