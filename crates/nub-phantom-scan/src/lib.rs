//! nub-phantom-scan — scan an already-extracted package version's PUBLISHED,
//! reachable code for UNDECLARED (phantom) dependencies, and reduce it to the
//! target-agnostic boolean the disk-eject decision needs.
//!
//! This is the shared home of the reachable-graph walk + verdict layer, lifted
//! out of the excluded `nub-phantom` eval tool so the shipped `nub` CLI's dynamic
//! per-version scan-on-link drives ejection from the SAME pipeline. The pipeline:
//!
//!   walk the module graph from `exports`/`main`/`bin` → extract import/require
//!   specifiers (via `nub-phantom-core`'s oxc parser) → classify each against the
//!   declared surface.
//!
//! For the dynamic detector the actionable output is [`ScanResult`]: a single
//! `has_unguarded_phantom` boolean (does this version statically, unguardedly
//! import a package it does not declare?) plus the offending target set with
//! provenance. Disk-eject is target-agnostic, so the boolean is all the eject
//! DECISION consults; the targets carry the provenance a later transitive
//! (subtree) reachability query would use.

pub mod classify;
pub mod graph;
pub mod manifest;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) use classify::Verdict;
use manifest::Manifest;

/// One undeclared target a scanned version pulls in, with the provenance bits the
/// eject reachability query needs. Serialized into the per-integrity sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhantomTarget {
    /// The undeclared package name (`zod`, `@apify/datastructures`).
    pub name: String,
    /// Reached from the package's main entry surface.
    from_main: bool,
    /// Reached from a non-`.` `exports` subpath (the adapter surface).
    from_subpath: bool,
}

/// The per-version scan verdict — the immutable, per-content-integrity payload
/// the dynamic detector caches and feeds to disk-eject.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanResult {
    /// The eject DECISION: at least one HARD (unguarded) undeclared import in the
    /// reachable published graph. Disk-eject is target-agnostic, so this boolean
    /// is what the eject decision consults.
    pub has_unguarded_phantom: bool,
    /// The offending undeclared targets (empty when `has_unguarded_phantom` is
    /// false). Provenance-tagged for the later transitive/subtree reachability
    /// query; the per-package eject decision itself needs only the boolean.
    pub targets: Vec<PhantomTarget>,
    /// DECLARED PEERS this package imports from its `.d.ts` TYPE surface — the
    /// nub#450 peer-type class. Distinct from `targets` (undeclared phantoms): a
    /// peer is DECLARED, so it is not a phantom, but under the global virtual store
    /// its `@types/<peer>` (a separate top-level package) is unreachable from the
    /// package's store realpath, so the type-checker loses the peer's types. The
    /// CONSUMER ejects this package only when the project actually has a top-level
    /// `@types/<peer>` (see `phantom_closure`), which both fixes it and bounds the
    /// eject to the peer-typed set. `#[serde(default)]` so a pre-field sidecar
    /// deserializes (the scanner-version bump re-scans it anyway).
    #[serde(default)]
    pub type_coupled_peers: Vec<String>,
    /// Reachable files parsed (diagnostic — lets the caller see scan breadth).
    files_analyzed: usize,
}

/// Scan an already-extracted package tree rooted at `root` (the dir holding
/// `package.json`) and reduce to a [`ScanResult`]. A tree that can't be parsed
/// (no/!readable `package.json`) yields `None` — the caller treats it as
/// "nothing to eject" (a scan miss must never itself force materialization).
pub fn scan_extracted(root: &Path) -> Option<ScanResult> {
    let raw = std::fs::read(root.join("package.json")).ok()?;
    let manifest = Manifest::parse(&raw)?;
    let walk = graph::walk(root, &manifest.entry_points);
    Some(reduce(&manifest, &walk))
}

/// Scan a package straight from its CAS-backed file index — the EXTRACT-TIME
/// entry, run inside the store's tarball import before any navigable tree
/// exists. `files` are `(package-relative-path, absolute CAS-blob-path)` pairs
/// projected from a `PackageIndex`: resolution runs over the relpath key set and
/// content is read from the paired blob. Returns the SAME [`ScanResult`] as
/// [`scan_extracted`] over the equivalent extracted tree (both reduce a shared
/// [`graph`] walk). `None` when the package has no parseable `package.json`.
pub fn scan_index(files: &[(String, PathBuf)]) -> Option<ScanResult> {
    let pkg_json = files.iter().find(|(rel, _)| rel == "package.json")?;
    let raw = std::fs::read(&pkg_json.1).ok()?;
    let manifest = Manifest::parse(&raw)?;
    let walk = graph::walk_index(files, &manifest.entry_points);
    Some(reduce(&manifest, &walk))
}

/// Reduce a completed reachable-graph walk to the eject [`ScanResult`]: classify
/// each reference, keep the HARD (unguarded) undeclared ones as targets, and set
/// the boolean. The single reduction shared by both scan entries — output
/// identity of `scan_index` and `scan_extracted` rests on this.
fn reduce(manifest: &Manifest, walk: &graph::Walk) -> ScanResult {
    let findings = classify::classify(manifest, &walk.references);
    let targets: Vec<PhantomTarget> = findings
        .iter()
        .filter(|f| f.verdict == Verdict::HardPhantom)
        .map(|f| PhantomTarget {
            name: f.package.clone(),
            from_main: f.from_main,
            from_subpath: f.from_subpath,
        })
        .collect();
    // Declared peers imported from the `.d.ts` type surface (nub#450). A peer is
    // DECLARED (not a phantom → not in `targets`), but its `@types/<peer>` is a
    // separate top-level package the store realpath can't reach; the consumer
    // decides the eject against the real project graph.
    let type_coupled_peers: Vec<String> = findings
        .iter()
        .filter(|f| {
            f.from_types
                && matches!(
                    f.verdict,
                    Verdict::DeclaredPeer | Verdict::DeclaredOptionalPeer
                )
        })
        .map(|f| f.package.clone())
        .collect();
    ScanResult {
        has_unguarded_phantom: !targets.is_empty(),
        targets,
        type_coupled_peers,
        files_analyzed: walk.files_analyzed,
    }
}

#[cfg(test)]
mod tests {
    use super::{PathBuf, scan_extracted, scan_index};
    use std::fs;

    /// `(relpath, blob-path)` pairs for an on-disk tree — the blob is the real
    /// file, so `scan_index` reads byte-identical content to `scan_extracted`.
    fn index_of(root: &std::path::Path) -> Vec<(String, PathBuf)> {
        fn rec(base: &std::path::Path, cur: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
            for e in fs::read_dir(cur).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    rec(base, &p, out);
                } else {
                    let rel = p
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, p));
                }
            }
        }
        let mut out = Vec::new();
        rec(root, root, &mut out);
        out
    }

    fn fixture() -> std::path::PathBuf {
        // Per-call unique dir: several tests build a fixture concurrently under
        // cargo's parallel runner, so a shared `process::id()`-only path would
        // let one test's cleanup wipe another's tree mid-scan.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "nub-phantom-scan-e2e-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{
                "name": "demo",
                "main": "index.js",
                "exports": { ".": "./index.js", "./adapter": "./adapter.js" },
                "dependencies": { "declared-dep": "1" },
                "peerDependencies": { "zod": "*" },
                "peerDependenciesMeta": { "zod": { "optional": true } }
            }"#,
        )
        .unwrap();
        fs::write(
            root.join("index.js"),
            r#"const a = require('declared-dep');
               const ghost = require('undeclared-ghost');
               let opt; try { opt = require('soft-ghost'); } catch {}
               require('./reached');"#,
        )
        .unwrap();
        fs::write(root.join("reached.js"), "import x from 'reached-ghost';").unwrap();
        fs::write(root.join("adapter.js"), "import 'backend-lib';").unwrap();
        root
    }

    #[test]
    fn scan_result_flags_hard_phantoms_only() {
        let root = fixture();
        let r = scan_extracted(&root).unwrap();
        assert!(r.has_unguarded_phantom);
        let names: Vec<&str> = r.targets.iter().map(|t| t.name.as_str()).collect();
        // Hard phantoms: undeclared-ghost, reached-ghost, backend-lib. NOT the
        // optional peer (zod), NOT the soft/try-guarded (soft-ghost).
        assert!(names.contains(&"undeclared-ghost"));
        assert!(names.contains(&"reached-ghost"));
        assert!(names.contains(&"backend-lib"));
        assert!(!names.contains(&"soft-ghost"));
        assert!(!names.contains(&"zod"));
        // Provenance: backend-lib is subpath-only (the adapter class).
        let backend = r.targets.iter().find(|t| t.name == "backend-lib").unwrap();
        assert!(backend.from_subpath && !backend.from_main);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dts_peer_type_surface_reports_type_coupled_peer() {
        // react-pdf shape (nub#450): declares `react` a PEER, ships no `types`
        // field, and its default `index.d.ts` imports react. The peer is DECLARED
        // (not an undeclared phantom → no target, has_unguarded_phantom false), but
        // it surfaces as a type-coupled peer so the consumer can eject on a
        // top-level `@types/react`. A relative `.d.ts` re-export is followed.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "nub-phantom-dts-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"@react-pdf/renderer","main":"./index.js","peerDependencies":{"react":"*"}}"#,
        )
        .unwrap();
        fs::write(root.join("index.js"), "module.exports = {};").unwrap();
        fs::write(
            root.join("index.d.ts"),
            "import * as React from 'react';\nexport type { Doc } from './types';\n",
        )
        .unwrap();
        fs::write(
            root.join("types.d.ts"),
            "import type { ReactNode } from 'react';\nexport type Doc = ReactNode;\n",
        )
        .unwrap();

        let r = scan_extracted(&root).unwrap();
        assert!(
            r.type_coupled_peers.contains(&"react".to_string()),
            "declared peer imported from the .d.ts surface is a type-coupled peer: {:?}",
            r.type_coupled_peers
        );
        assert!(
            !r.has_unguarded_phantom,
            "a declared peer is NOT an undeclared phantom"
        );
        assert!(
            r.targets.is_empty(),
            "no undeclared targets: {:?}",
            r.targets
        );
        // Output-identical between the two scan entries (the hard invariant).
        assert_eq!(scan_index(&index_of(&root)).unwrap(), r);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn type_walk_does_not_leak_runtime_imports_from_a_js_sibling() {
        // Finding-1 regression: a `.d.ts` re-exports `./widgets`, and the standard
        // compiled layout ships BOTH `widgets.js` and `widgets.d.ts`. The type walk
        // must resolve `./widgets` to `widgets.d.ts` (NOT `widgets.js`), so
        // `widgets.js`'s runtime `require('react')` is NOT captured as a type-peer
        // (no spurious eject) and the real `widgets.d.ts` IS walked.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "nub-phantom-dtsleak-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"ui-lib","main":"./index.js","peerDependencies":{"react":"*"}}"#,
        )
        .unwrap();
        fs::write(
            root.join("index.js"),
            "module.exports = require('./widgets');",
        )
        .unwrap();
        fs::write(root.join("index.d.ts"), "export * from './widgets';\n").unwrap();
        // widgets.js runtime-requires react; widgets.d.ts does NOT type-import it.
        fs::write(
            root.join("widgets.js"),
            "const React = require('react');\nmodule.exports = {};",
        )
        .unwrap();
        fs::write(
            root.join("widgets.d.ts"),
            "import type { Backend } from 'the-backend';\nexport declare const Widget: Backend;\n",
        )
        .unwrap();

        let r = scan_extracted(&root).unwrap();
        assert!(
            !r.type_coupled_peers.contains(&"react".to_string()),
            "react (runtime-only, in widgets.js) must NOT leak into type_coupled_peers: {:?}",
            r.type_coupled_peers
        );
        // The real widgets.d.ts WAS walked: its undeclared type import surfaced.
        assert!(
            r.targets.iter().any(|t| t.name == "the-backend"),
            "widgets.d.ts should be walked from the type surface: {:?}",
            r.targets
        );
        assert_eq!(scan_index(&index_of(&root)).unwrap(), r);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_package_has_no_phantom() {
        let root =
            std::env::temp_dir().join(format!("nub-phantom-scan-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"clean","main":"index.js","dependencies":{"lodash":"1"}}"#,
        )
        .unwrap();
        fs::write(
            root.join("index.js"),
            "const _ = require('lodash'); const fs = require('node:fs');",
        )
        .unwrap();
        let r = scan_extracted(&root).unwrap();
        assert!(!r.has_unguarded_phantom);
        assert!(r.targets.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extensionless_and_directory_main_are_scanned() {
        // Regression (the @vercel/static-build shape): a package whose `main` has
        // no file extension (`"./dist/index"`) or points at a directory
        // (`"./dist"`) must still be scanned. Before the entry-candidate fix the
        // `is_js_like`-only gate dropped these mains → `files_analyzed: 0` → the
        // phantom `require('@vercel/build-utils')` inside `dist/index.js` was never
        // seen, so the package silently ejected nothing and broke at runtime.
        let build = |main: &str| {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "nub-phantom-scan-extless-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("dist")).unwrap();
            fs::write(
                root.join("package.json"),
                format!(r#"{{"name":"pkg","main":"{main}","dependencies":{{"react":"*"}}}}"#),
            )
            .unwrap();
            fs::write(
                root.join("dist/index.js"),
                "const b = require('undeclared-phantom'); require('react');",
            )
            .unwrap();
            root
        };

        for main in ["./dist/index", "./dist"] {
            let root = build(main);
            let r = scan_extracted(&root).unwrap();
            assert!(
                r.files_analyzed > 0,
                "main {main:?}: entry must resolve and scan (files_analyzed > 0)"
            );
            assert!(
                r.has_unguarded_phantom,
                "main {main:?}: undeclared-phantom must be flagged"
            );
            assert!(
                r.targets.iter().any(|t| t.name == "undeclared-phantom"),
                "main {main:?}: phantom target present"
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn scan_index_is_output_identical_to_scan_extracted() {
        // The hard requirement: an extract-time index scan yields the exact same
        // verdict + target set + file count as a post-link tree scan. Uses the
        // full phantom fixture (main graph + `./adapter` subpath, hard/soft/peer
        // mix) so provenance and classification are both exercised.
        let root = fixture();
        let from_tree = scan_extracted(&root).unwrap();
        let from_index = scan_index(&index_of(&root)).unwrap();
        assert_eq!(
            from_index, from_tree,
            "extract-time index scan diverged from post-link tree scan"
        );
        assert!(from_index.has_unguarded_phantom);
        let _ = fs::remove_dir_all(&root);
    }
}
