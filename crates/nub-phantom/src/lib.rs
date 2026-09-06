//! nub-phantom — detect UNDECLARED (phantom) dependencies of an npm package.
//!
//! A phantom is a bare `import`/`require` in a package's PUBLISHED, reachable
//! code that is not covered by any of: the package's own `dependencies` /
//! `optionalDependencies` / `peerDependencies` (including optional peers), Node
//! builtins, a self reference, or a bundled dep. The pipeline:
//!
//!   fetch tarball → walk the module graph from `exports`/`main`/`bin` → extract
//!   import/require specifiers (oxc) → classify each against the declared surface.
//!
//! The classifier's whole job is to NOT false-flag: declared optional peers, soft
//! (try/catch) loads, type-only imports, and unreached dev files are each handled
//! so the emitted phantom set is trustworthy enough to drive the vendored
//! `packageExtensions`/disk-materialize list.

pub mod fetch;

// The reachable-graph walk, classifier, and manifest parser are shared with the
// shipped `nub` CLI via `nub-phantom-scan` (single source of truth). This eval
// tool once carried its own byte-identical copies of these modules, which could
// silently diverge from the shipped scan; re-exporting keeps the in-crate
// `crate::classify` / `crate::graph` / `crate::manifest` paths resolving to the
// one implementation. The specifier/extraction primitives underneath come from
// `nub-phantom-core`, which `nub-phantom-scan` depends on transitively.
pub use nub_phantom_scan::{classify, graph, manifest};

use std::path::Path;

use serde::Serialize;

use classify::{Finding, Verdict};
use manifest::Manifest;

/// The full analysis of one package.
#[derive(Debug, Serialize)]
pub struct PackageReport {
    pub name: String,
    pub version: String,
    pub findings: Vec<Finding>,
    pub files_analyzed: usize,
    pub unresolved_relative: usize,
}

impl PackageReport {
    /// Count of findings with a given verdict.
    pub fn count(&self, v: Verdict) -> usize {
        self.findings.iter().filter(|f| f.verdict == v).count()
    }

    /// Genuine hard phantoms — the actionable set.
    pub fn hard_phantoms(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.verdict == Verdict::HardPhantom)
    }

    /// Hard phantoms of the LEGACY DEEP-PATH class — reached only through a
    /// published file no declared surface references, which Node's legacy
    /// resolution still lets a consumer import (`redux-persist/lib/integration/
    /// react` → `react`). Separable so a downstream consumer can weigh them
    /// against `main`-reachable phantoms rather than merging the two.
    pub fn deep_path_phantoms(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.is_deep_path_only())
    }

    /// Hard phantoms of the subpath-adapter class (subpath-only) — the
    /// consumer-provided-backend pattern that breaks under GVS-default.
    pub fn subpath_adapter_phantoms(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.is_subpath_adapter())
    }

    /// What a NAIVE detector would flag: every undeclared reference, NOT excluding
    /// optional peers or soft loads. Lets the report quantify the over-count the
    /// real classifier avoids. (Builtins/self/declared are excluded even naively;
    /// the over-count includes optional peers, soft loads, and type-only imports.)
    pub fn naive_phantom_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| {
                matches!(
                    f.verdict,
                    Verdict::HardPhantom
                        | Verdict::SoftPhantom
                        | Verdict::DeclaredOptionalPeer
                        | Verdict::TypeOnly
                )
            })
            .count()
    }
}

/// Analyze an already-extracted package tree rooted at `root` (the dir holding
/// `package.json`). Separated from `fetch` so it is testable offline.
pub fn analyze_extracted(root: &Path, version: &str) -> Result<PackageReport, String> {
    let raw =
        std::fs::read(root.join("package.json")).map_err(|e| format!("read package.json: {e}"))?;
    let manifest = Manifest::parse(&raw).ok_or("unparseable package.json / no name")?;
    // An `exports` map is the authoritative public surface, so a package that has
    // one is walked from it alone. Without one, Node's legacy resolution lets a
    // consumer import ANY published file by deep path — `redux-persist` ships no
    // `exports`, and its `lib/integration/react.js` (a documented import target)
    // is reached by nothing from `main`, so its undeclared `require("react")` was
    // invisible. The eval tool opts in; the shipped disk-eject scan does not (see
    // `nub_phantom_scan::scan_extracted`), because these findings are speculative
    // and the eject decision must not act on them unreviewed.
    let opts = graph::WalkOptions {
        deep_path_roots: !manifest.has_exports,
    };
    let walk = graph::walk_with(root, &manifest.entry_points, opts);
    let findings = classify::classify(&manifest, &walk.references);
    Ok(PackageReport {
        name: manifest.name,
        version: version.to_string(),
        findings,
        files_analyzed: walk.files_analyzed,
        unresolved_relative: walk.unresolved_relative,
    })
}

/// Fetch `name`'s latest tarball and analyze it.
pub fn analyze(client: &reqwest::blocking::Client, name: &str) -> Result<PackageReport, String> {
    let pkg = fetch::fetch(client, name)?;
    analyze_extracted(&pkg.root, &pkg.version)
}

#[cfg(test)]
mod tests {
    use super::analyze_extracted;
    use crate::classify::Verdict;
    use std::fs;

    /// The legacy deep-path class, end to end and both ways round: a package with
    /// NO `exports` map has every published file speculatively seeded (the
    /// redux-persist@6.0.0 miss), while the SAME tree with an `exports` map keeps
    /// its unexported internals out of the walk — `exports` is Node's encapsulation
    /// boundary, so a file it does not name cannot be imported and must not
    /// manufacture a phantom.
    #[test]
    fn deep_path_roots_apply_only_without_an_exports_map() {
        let build = |manifest: &str| {
            let root = std::env::temp_dir().join(format!(
                "nub-phantom-deep-{}-{:x}",
                std::process::id(),
                manifest.len()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("lib/integration")).unwrap();
            fs::create_dir_all(root.join("test")).unwrap();
            fs::write(root.join("package.json"), manifest).unwrap();
            fs::write(root.join("lib/index.js"), "module.exports = {};").unwrap();
            // Reachable only by `<pkg>/lib/integration/react` — a documented import
            // target that `main` never references.
            fs::write(
                root.join("lib/integration/react.js"),
                "var r = require('react');",
            )
            .unwrap();
            // Ships in the tarball, imports a devDep: must stay invisible either way.
            fs::write(root.join("test/index.js"), "require('ava');").unwrap();
            root
        };

        let legacy =
            build(r#"{"name":"demo","main":"lib/index.js","devDependencies":{"ava":"1"}}"#);
        let r = analyze_extracted(&legacy, "0.0.0").unwrap();
        let f = r.findings.iter().find(|f| f.package == "react").unwrap();
        assert_eq!(f.verdict, Verdict::HardPhantom);
        assert!(
            f.is_deep_path_only(),
            "flagged as the deep-path class: {f:?}"
        );
        assert_eq!(r.deep_path_phantoms().count(), 1);
        assert!(
            !r.findings.iter().any(|f| f.package == "ava"),
            "test/ never seeds the walk: {:?}",
            r.findings
        );
        let _ = fs::remove_dir_all(&legacy);

        let encapsulated = build(
            r#"{"name":"demo","exports":{".":"./lib/index.js"},"devDependencies":{"ava":"1"}}"#,
        );
        let r = analyze_extracted(&encapsulated, "0.0.0").unwrap();
        assert!(
            r.findings.is_empty(),
            "an `exports` map bars deep imports — no speculative roots: {:?}",
            r.findings
        );
        let _ = fs::remove_dir_all(&encapsulated);
    }

    /// End-to-end over a hand-built package tree: exercises the whole pipeline —
    /// entry resolution, graph walk, extraction, and classification of every
    /// tricky category at once.
    #[test]
    fn end_to_end_classifies_every_category() {
        let root = std::env::temp_dir().join(format!("nub-phantom-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{
                "name": "demo",
                "main": "index.js",
                "exports": { ".": "./index.js", "./adapter": "./adapter.js" },
                "dependencies": { "declared-dep": "1" },
                "devDependencies": { "test-only": "1" },
                "peerDependencies": { "react": "*", "zod": "*" },
                "peerDependenciesMeta": { "zod": { "optional": true } }
            }"#,
        )
        .unwrap();
        // Reachable entry. Pulls in a declared dep, a builtin, an optional peer,
        // a required peer, a hard phantom, a soft phantom, a self ref, and a
        // relative edge to a reached module.
        fs::write(
            root.join("index.js"),
            r#"
            const a = require('declared-dep');
            const fs = require('node:fs');
            const path = require('path');
            const zod = require('zod');
            const react = require('react');
            const ghost = require('undeclared-ghost');
            const self = require('demo/sub');
            let opt; try { opt = require('soft-ghost'); } catch {}
            require('./reached');
            "#,
        )
        .unwrap();
        // Reached module imports another hard phantom.
        fs::write(root.join("reached.js"), "import x from 'reached-ghost';").unwrap();
        // The `./adapter` subpath statically imports an undeclared, consumer-
        // provided backend — the subpath-adapter class (the GVS-default bug shape).
        fs::write(root.join("adapter.js"), "import 'backend-lib';").unwrap();
        // NOT reached: would add a false phantom if the whole repo were scanned.
        fs::write(root.join("test.js"), "require('test-only');").unwrap();

        let r = analyze_extracted(&root, "0.0.0").unwrap();
        let v = |name: &str| {
            r.findings
                .iter()
                .find(|f| f.package == name)
                .map(|f| f.verdict)
        };

        assert_eq!(v("declared-dep"), Some(Verdict::Declared));
        assert_eq!(
            v("node:fs"),
            Some(Verdict::Builtin),
            "node: prefix is a builtin"
        );
        assert_eq!(v("path"), Some(Verdict::Builtin), "bare builtin");
        assert_eq!(v("zod"), Some(Verdict::DeclaredOptionalPeer));
        assert_eq!(v("react"), Some(Verdict::DeclaredPeer));
        assert_eq!(v("undeclared-ghost"), Some(Verdict::HardPhantom));
        assert_eq!(v("reached-ghost"), Some(Verdict::HardPhantom));
        assert_eq!(v("soft-ghost"), Some(Verdict::SoftPhantom));
        assert_eq!(v("demo"), Some(Verdict::SelfRef));
        assert_eq!(v("test-only"), None, "unreached dev import must not appear");
        assert_eq!(v("backend-lib"), Some(Verdict::HardPhantom));

        assert_eq!(r.count(Verdict::HardPhantom), 3);
        // Naive count would also flag zod (optional peer) + soft-ghost → 5; the
        // real classifier's actionable phantom set is 3.
        assert_eq!(r.naive_phantom_count(), 5);

        // Only `backend-lib` is the subpath-adapter class (subpath-only). The
        // main-graph phantoms (undeclared-ghost, reached-ghost) are not.
        let adapters: Vec<&str> = r
            .findings
            .iter()
            .filter(|f| f.is_subpath_adapter())
            .map(|f| f.package.as_str())
            .collect();
        assert_eq!(adapters, vec!["backend-lib"]);

        let _ = fs::remove_dir_all(&root);
    }
}
