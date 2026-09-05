//! Classify each referenced package against the manifest's declared surface.
//!
//! Aggregation rule: a package is HARD-needed if it is referenced by at least one
//! UNGUARDED occurrence; it is soft only if EVERY occurrence is guarded (in a
//! try/catch). The classification then answers the one question that matters —
//! is this reference covered by something a consumer install makes resolvable?

use std::collections::BTreeMap;

use serde::Serialize;

use crate::graph::Reference;
use crate::manifest::Manifest;
use nub_phantom_core::builtins::is_builtin;

/// The verdict for one referenced package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Undeclared and hard-required — a genuine phantom dependency.
    HardPhantom,
    /// Undeclared but only ever loaded under a try/catch — a soft/optional load,
    /// not a hard break.
    SoftPhantom,
    /// Declared as an OPTIONAL peer (`peerDependenciesMeta.<x>.optional`). NOT a
    /// phantom — the pick-your-plugin pattern. Tracked so the report can show how
    /// much a naive scan over-counts.
    DeclaredOptionalPeer,
    /// Declared as a required peer.
    DeclaredPeer,
    /// Declared in `dependencies`/`optionalDependencies`, or bundled.
    Declared,
    /// A Node builtin.
    Builtin,
    /// A self reference (the package's own name / subpath).
    SelfRef,
    /// Undeclared as a runtime dep, but present in `devDependencies` AND reachable
    /// only as a speculative legacy deep-path root. That combination describes a
    /// build/test helper that shipped in the tarball: nothing on the published
    /// surface reaches it, and the author's own manifest says the import is
    /// dev-time. Reported as its own category rather than a phantom — the
    /// deep-path class is speculative, so it does not get to override the author's
    /// declaration. A devDep import reached from `main`/`bin`/`exports` is
    /// unaffected and stays a phantom.
    DevOnlyDeepPath,
}

/// One classified package reference.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub package: String,
    pub verdict: Verdict,
    /// True if every occurrence was guarded (try/catch or a conditional branch).
    soft: bool,
    /// Reachable from the package's main entry surface.
    pub(crate) from_main: bool,
    /// Reachable from a non-`.` `exports` subpath (the adapter surface).
    pub(crate) from_subpath: bool,
    /// Reachable from the `.d.ts` TYPE surface — a DECLARED PEER with this set is
    /// the nub#450 peer-type class (its `@types/<peer>` must be project-local).
    pub(crate) from_types: bool,
    /// Reachable from a speculative legacy deep-path root — a published file no
    /// declared surface references, in a package with no `exports` map. Read
    /// across the crate boundary through [`Finding::is_deep_path_only`], as the
    /// other provenance bits are through [`Finding::is_subpath_adapter`].
    pub(crate) from_deep_path: bool,
    /// Example raw specifiers (deduped) showing how it was referenced.
    pub specifiers: Vec<String>,
}

impl Finding {
    /// The subpath-adapter class the GVS-default bug hinges on: a HARD phantom
    /// reachable ONLY from a non-`.` `exports` subpath (not the main graph). This
    /// is the `<pkg>/<adapter>` that statically imports a consumer-installed
    /// backend it never declares (`@hookform/resolvers/zod` → `zod`).
    pub fn is_subpath_adapter(&self) -> bool {
        self.verdict == Verdict::HardPhantom && self.from_subpath && !self.from_main
    }

    /// The legacy deep-path class: a HARD phantom reached ONLY through a published
    /// file that no declared surface references (`redux-persist/lib/integration/
    /// react` → `react`). Node's legacy resolution makes it genuinely importable,
    /// but nothing in the manifest says a consumer does — so it is real-but-
    /// lower-confidence, and a downstream consumer should be able to weigh it
    /// separately from a `main`-reachable phantom.
    pub fn is_deep_path_only(&self) -> bool {
        self.verdict == Verdict::HardPhantom
            && self.from_deep_path
            && !self.from_main
            && !self.from_subpath
            && !self.from_types
    }
}

/// Classify all references against `manifest`. Returns one `Finding` per distinct
/// referenced package, sorted by package name.
pub fn classify(manifest: &Manifest, references: &[Reference]) -> Vec<Finding> {
    // Aggregate per package: soft-ness ANDs (hard wins), provenance ORs, collect
    // example specs.
    struct Agg {
        all_soft: bool,
        from_main: bool,
        from_subpath: bool,
        from_types: bool,
        from_deep_path: bool,
        specs: Vec<String>,
    }
    let mut by_pkg: BTreeMap<String, Agg> = BTreeMap::new();
    for r in references {
        let e = by_pkg.entry(r.package.clone()).or_insert(Agg {
            all_soft: true,
            from_main: false,
            from_subpath: false,
            from_types: false,
            from_deep_path: false,
            specs: Vec::new(),
        });
        e.all_soft &= r.soft;
        e.from_main |= r.from_main;
        e.from_subpath |= r.from_subpath;
        e.from_types |= r.from_types;
        e.from_deep_path |= r.from_deep_path;
        if !e.specs.contains(&r.raw) {
            e.specs.push(r.raw.clone());
        }
    }

    by_pkg
        .into_iter()
        .map(|(package, agg)| {
            let deep_path_only =
                agg.from_deep_path && !agg.from_main && !agg.from_subpath && !agg.from_types;
            let verdict = verdict_for(manifest, &package, agg.all_soft, deep_path_only);
            Finding {
                package,
                verdict,
                soft: agg.all_soft,
                from_main: agg.from_main,
                from_subpath: agg.from_subpath,
                from_types: agg.from_types,
                from_deep_path: agg.from_deep_path,
                specifiers: agg.specs,
            }
        })
        .collect()
}

fn verdict_for(
    manifest: &Manifest,
    package: &str,
    all_soft: bool,
    deep_path_only: bool,
) -> Verdict {
    if is_self(manifest, package) {
        return Verdict::SelfRef;
    }
    if is_builtin(package) {
        return Verdict::Builtin;
    }
    if manifest.deps.contains(package) || manifest.bundled.contains(package) {
        return Verdict::Declared;
    }
    if manifest.optional_peers.contains(package) {
        return Verdict::DeclaredOptionalPeer;
    }
    if manifest.required_peers.contains(package) {
        return Verdict::DeclaredPeer;
    }
    // Undeclared. A speculative deep-path root importing one of the package's own
    // devDependencies is the shipped-build-helper shape, not a consumer surface —
    // the author declared the import dev-time and nothing published reaches the
    // file, so a speculative root does not get to promote it to a phantom.
    if deep_path_only && manifest.dev_deps.contains(package) {
        return Verdict::DevOnlyDeepPath;
    }
    if all_soft {
        Verdict::SoftPhantom
    } else {
        Verdict::HardPhantom
    }
}

/// A reference to the package's own name is a self import (resolvable via the
/// package's own `exports`), never a phantom.
fn is_self(manifest: &Manifest, package: &str) -> bool {
    package == manifest.name
}

#[cfg(test)]
mod tests {
    use super::{Verdict, classify};
    use crate::graph::Reference;
    use crate::manifest::Manifest;

    fn refs(items: &[(&str, &str, bool)]) -> Vec<Reference> {
        items
            .iter()
            .map(|(p, raw, soft)| Reference {
                package: (*p).to_string(),
                raw: (*raw).to_string(),
                soft: *soft,
                from_main: true,
                from_subpath: false,
                from_types: false,
                from_deep_path: false,
            })
            .collect()
    }

    #[test]
    fn declared_optional_peer_is_not_a_phantom() {
        // @hookform/resolvers-style: zod is a DECLARED optional peer, referenced
        // by the /zod subpath. Must NOT be flagged phantom.
        let m = Manifest::parse(
            br#"{"name":"@hookform/resolvers","peerDependencies":{"zod":"*"},
                 "peerDependenciesMeta":{"zod":{"optional":true}}}"#,
        )
        .unwrap();
        let f = classify(&m, &refs(&[("zod", "zod", false)]));
        assert_eq!(f[0].verdict, Verdict::DeclaredOptionalPeer);
    }

    #[test]
    fn undeclared_hard_require_is_a_phantom_soft_is_not() {
        let m = Manifest::parse(br#"{"name":"pkg","dependencies":{"a":"1"}}"#).unwrap();
        let f = classify(
            &m,
            &refs(&[
                ("a", "a", false),         // declared
                ("ghost", "ghost", false), // hard phantom
                ("maybe", "maybe", true),  // soft phantom
                ("fs", "fs", false),       // builtin
            ]),
        );
        let v = |name: &str| f.iter().find(|x| x.package == name).unwrap().verdict;
        assert_eq!(v("a"), Verdict::Declared);
        assert_eq!(v("ghost"), Verdict::HardPhantom);
        assert_eq!(v("maybe"), Verdict::SoftPhantom);
        assert_eq!(v("fs"), Verdict::Builtin);
    }

    #[test]
    fn one_hard_occurrence_beats_a_soft_one() {
        // Same undeclared package referenced both guarded and unguarded → hard.
        let m = Manifest::parse(br#"{"name":"pkg"}"#).unwrap();
        let f = classify(&m, &refs(&[("x", "x", true), ("x", "x/sub", false)]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].verdict, Verdict::HardPhantom);
        assert!(!f[0].soft);
    }

    #[test]
    fn deep_path_only_devdep_is_not_a_phantom_but_a_reached_one_is() {
        // A speculative deep-path root importing one of the package's OWN
        // devDependencies is a shipped build/test helper — the author declared the
        // import dev-time and no published surface reaches the file, so the
        // speculative root does not get to promote it to a phantom. The same
        // devDep reached from `main` is unaffected and stays hard.
        let m = Manifest::parse(br#"{"name":"pkg","devDependencies":{"ava":"1"}}"#).unwrap();
        let deep = |pkg: &str| Reference {
            package: pkg.to_string(),
            raw: pkg.to_string(),
            soft: false,
            from_main: false,
            from_subpath: false,
            from_types: false,
            from_deep_path: true,
        };
        let f = classify(&m, &[deep("ava"), deep("react")]);
        let v = |name: &str| f.iter().find(|x| x.package == name).unwrap();
        assert_eq!(v("ava").verdict, Verdict::DevOnlyDeepPath);
        assert_eq!(
            v("react").verdict,
            Verdict::HardPhantom,
            "an undeclared package reached only by deep path is still a phantom"
        );
        assert!(v("react").is_deep_path_only());
        assert!(!v("ava").is_deep_path_only());

        // Same devDep, reached from `main` as well → the guard does not apply.
        let mut also_main = deep("ava");
        also_main.from_main = true;
        let f = classify(&m, &[deep("ava"), also_main]);
        assert_eq!(f[0].verdict, Verdict::HardPhantom);
        assert!(!f[0].is_deep_path_only());
    }

    #[test]
    fn subpath_only_hard_phantom_is_the_adapter_class() {
        let m = Manifest::parse(br#"{"name":"@hookform/resolvers"}"#).unwrap();
        // A hard phantom reached only from a subpath export is the adapter class;
        // one reached from the main graph is not.
        let subpath_only = Reference {
            package: "zod".into(),
            raw: "zod/v4/core".into(),
            soft: false,
            from_main: false,
            from_subpath: true,
            from_types: false,
            from_deep_path: false,
        };
        let main_reached = Reference {
            package: "junk".into(),
            raw: "junk".into(),
            soft: false,
            from_main: true,
            from_subpath: false,
            from_types: false,
            from_deep_path: false,
        };
        let f = classify(&m, &[subpath_only, main_reached]);
        let zod = f.iter().find(|x| x.package == "zod").unwrap();
        let junk = f.iter().find(|x| x.package == "junk").unwrap();
        assert!(zod.is_subpath_adapter());
        assert!(!junk.is_subpath_adapter());
    }
}
