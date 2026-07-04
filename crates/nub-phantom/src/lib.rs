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
//! `packageExtensions`/force-materialize list.

pub mod classify;
pub mod fetch;
pub mod graph;
pub mod manifest;

// The specifier-extraction + classification primitives are shared with the
// shipped `nub` CLI via `nub-phantom-core` (single source of truth — the guard
// modeling and builtin/name classification live there, not duplicated here).
// Re-exported so the in-crate `crate::builtins` / `crate::extract` /
// `crate::specifier` paths resolve unchanged.
pub use nub_phantom_core::{builtins, extract, specifier};

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

    /// Hard phantoms of the subpath-adapter class (subpath-only) — the
    /// consumer-provided-backend pattern that breaks under GVS-default.
    pub fn subpath_adapter_phantoms(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.is_subpath_adapter())
    }

    /// What a NAIVE detector would flag: every undeclared reference, NOT excluding
    /// optional peers or soft loads. Lets the report quantify the over-count the
    /// real classifier avoids. (Builtins/self/declared are excluded even naively;
    /// the over-count is optional-peers + soft-phantoms counted as phantoms.)
    pub fn naive_phantom_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| {
                matches!(
                    f.verdict,
                    Verdict::HardPhantom | Verdict::SoftPhantom | Verdict::DeclaredOptionalPeer
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
    let walk = graph::walk(root, &manifest.entry_points);
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
