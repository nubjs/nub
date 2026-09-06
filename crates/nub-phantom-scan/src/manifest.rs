//! Parse a package's `package.json` into (a) its DECLARED dependency surface —
//! the sets a specifier is checked against — and (b) its published ENTRY POINTS,
//! the roots of the reachable-module walk.
//!
//! The declared surface deliberately mirrors what a consumer install actually
//! makes resolvable at runtime: `dependencies`, `optionalDependencies`, and
//! `peerDependencies` (split by the `peerDependenciesMeta.<x>.optional` flag),
//! plus bundled deps. `devDependencies` are intentionally EXCLUDED — they are not
//! installed for consumers, so an import of one from published code is a phantom.

use std::collections::BTreeSet;

use serde_json::Value;

/// The declared dependency surface a specifier is classified against.
#[derive(Debug, Default)]
pub struct Manifest {
    pub name: String,
    /// `dependencies` ∪ `optionalDependencies` — hard-declared, always resolvable.
    pub(crate) deps: BTreeSet<String>,
    /// Required peers (`peerDependencies` without an `optional` meta flag).
    pub(crate) required_peers: BTreeSet<String>,
    /// Optional peers (`peerDependenciesMeta.<x>.optional === true`). Declared —
    /// NOT phantoms — but reported as their own category (the pick-your-plugin
    /// pattern that a naive detector over-flags).
    pub(crate) optional_peers: BTreeSet<String>,
    /// `bundledDependencies` / `bundleDependencies` — shipped inside the tarball.
    pub(crate) bundled: BTreeSet<String>,
    /// `devDependencies`. NOT part of the declared surface — a consumer install
    /// does not make them resolvable, so importing one from published code is a
    /// phantom. Kept only to discriminate the SPECULATIVE deep-path class: a file
    /// reachable by nothing but a deep path, importing a package the author did
    /// declare as a devDep, is a shipped build/test helper rather than a public
    /// surface (see [`crate::classify`]).
    pub(crate) dev_deps: BTreeSet<String>,
    /// Whether the manifest declares an `exports` map. An `exports` map is Node's
    /// encapsulation boundary: it is the AUTHORITATIVE public surface, and a file
    /// it does not name genuinely cannot be imported. Without one, Node's legacy
    /// resolution lets a consumer `require('<pkg>/<any/published/file.js>')`, so
    /// every published JS file is a potential entry point.
    pub has_exports: bool,
    /// Published entry files (relative paths from the package root) — the roots
    /// of the reachable-module walk, each tagged by whether it is the main entry
    /// or a non-`.` `exports` subpath (the adapter surface).
    pub entry_points: Vec<Entry>,
}

/// Which published surface an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// `main` / `module` / `bin` / the `exports."."` root.
    Main,
    /// A non-`.` `exports` subpath (`./zod`, `./vitest`) — the adapter surface a
    /// consumer opts into by importing `<pkg>/<subpath>`.
    Subpath,
    /// A `.d.ts` TYPE surface (`types`/`typings` field, an `exports` `types`
    /// condition, or the `index.d.ts` default) — the root of the declaration-file
    /// graph. Reached references carry `from_types` so a type-position peer import
    /// (`import * as React from 'react'` in a `.d.ts`) is separable from a runtime
    /// one; this is what drives the nub#450 `@types/<peer>` reachability eject
    /// without touching runtime-phantom detection.
    Types,
}

/// A published entry file + its surface kind.
#[derive(Debug, Clone)]
pub struct Entry {
    pub(crate) path: String,
    pub(crate) kind: EntryKind,
}

impl Manifest {
    /// Parse from raw `package.json` bytes. Returns `None` if the JSON is
    /// unparseable or has no name (a package with no identity can't be analyzed).
    pub fn parse(raw: &[u8]) -> Option<Manifest> {
        let v: Value = serde_json::from_slice(raw).ok()?;
        let name = v.get("name")?.as_str()?.to_string();

        let mut m = Manifest {
            name: name.clone(),
            ..Default::default()
        };

        collect_keys(&v, "dependencies", &mut m.deps);
        collect_keys(&v, "optionalDependencies", &mut m.deps);
        collect_keys(&v, "peerDependencies", &mut m.required_peers);

        // Move any peer flagged optional out of required_peers into optional_peers.
        if let Some(meta) = v.get("peerDependenciesMeta").and_then(Value::as_object) {
            for (peer, cfg) in meta {
                let optional = cfg
                    .get("optional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if optional {
                    m.required_peers.remove(peer);
                    m.optional_peers.insert(peer.clone());
                }
            }
        }

        collect_keys(&v, "devDependencies", &mut m.dev_deps);
        collect_bundled(&v, &mut m.bundled);
        // `"exports": null` counts: it exports nothing, which is still an
        // encapsulation boundary, not a legacy deep-path-open package.
        m.has_exports = v.get("exports").is_some();
        m.entry_points = collect_entry_points(&v);
        Some(m)
    }
}

fn collect_keys(v: &Value, field: &str, out: &mut BTreeSet<String>) {
    if let Some(obj) = v.get(field).and_then(Value::as_object) {
        for k in obj.keys() {
            out.insert(k.clone());
        }
    }
}

/// `bundledDependencies`/`bundleDependencies` is an ARRAY of names (both spellings
/// are valid npm).
fn collect_bundled(v: &Value, out: &mut BTreeSet<String>) {
    for field in ["bundledDependencies", "bundleDependencies"] {
        if let Some(arr) = v.get(field).and_then(Value::as_array) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    out.insert(s.to_string());
                }
            }
        }
    }
}

/// Gather the published entry files from `main`, `module`, `bin`, and `exports`,
/// each tagged Main or Subpath, PLUS the `.d.ts` TYPE surface (`types`/`typings`,
/// `exports` `types` conditions, or the TS default roots) tagged Types. A
/// recognized JS file is kept directly; an extensionless / directory-style target
/// (`"./dist/index"`, `"./dist"`) is kept as a candidate and resolved Node-style
/// by the graph walk; non-JS asset conditions (`.json`/`.node`/`.wasm`/`.css`)
/// carry no analyzable imports and are filtered.
fn collect_entry_points(v: &Value) -> Vec<Entry> {
    // Dedup on (kind, path) so a file exported at both `.` and a subpath seeds
    // both surfaces into the walk.
    let mut seen: BTreeSet<(u8, String)> = BTreeSet::new();
    let mut out = Vec::new();
    let mut push =
        |p: &str, kind: EntryKind, out: &mut Vec<Entry>, seen: &mut BTreeSet<(u8, String)>| {
            let norm = normalize_rel(p);
            let key = (kind as u8, norm.clone());
            if is_entry_candidate(&norm) && seen.insert(key) {
                out.push(Entry { path: norm, kind });
            }
        };

    for field in ["main", "module"] {
        if let Some(s) = v.get(field).and_then(Value::as_str) {
            push(s, EntryKind::Main, &mut out, &mut seen);
        }
    }

    match v.get("bin") {
        Some(Value::String(s)) => push(s, EntryKind::Main, &mut out, &mut seen),
        Some(Value::Object(map)) => {
            for b in map.values() {
                if let Some(s) = b.as_str() {
                    push(s, EntryKind::Main, &mut out, &mut seen);
                }
            }
        }
        _ => {}
    }

    // The top level of `exports` decides the kind: a `.`-keyed subpath map splits
    // `.` (Main) from every `./x` (Subpath); a bare condition map or string is the
    // main entry (sugar for `.`).
    if let Some(exports) = v.get("exports") {
        match exports {
            Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => {
                for (k, child) in map {
                    let kind = if k == "." {
                        EntryKind::Main
                    } else {
                        EntryKind::Subpath
                    };
                    walk_exports(child, kind, &mut out, &mut seen, &mut push);
                }
            }
            other => walk_exports(other, EntryKind::Main, &mut out, &mut seen, &mut push),
        }
    }

    // Fallback: a package with no explicit entry resolves `./index.js` (Node's
    // default main). Give the walk a root so an entry-less legacy package is
    // still analyzed.
    if out.is_empty() {
        out.push(Entry {
            path: "index.js".to_string(),
            kind: EntryKind::Main,
        });
    }

    // TYPE-surface roots (`.d.ts`) — the explicit `types`/`typings` field and every
    // `exports` `types` condition, else the TS default roots. Seeded as
    // `EntryKind::Types` so reached references carry `from_types` (nub#450). Runtime
    // entry collection above is untouched.
    let mut type_targets: Vec<String> = Vec::new();
    for field in ["types", "typings"] {
        if let Some(s) = v.get(field).and_then(Value::as_str) {
            type_targets.push(s.to_string());
        }
    }
    if let Some(exports) = v.get("exports") {
        collect_export_types(exports, &mut type_targets);
    }
    if type_targets.is_empty() {
        // No explicit type surface → TS's defaults: each Main entry's colocated
        // declaration (`x.js` → `x.d.ts`, `./dist` → `./dist/index.d.ts`) plus the
        // root `index.d.ts`. The Types-surface resolver stems the runtime extension
        // and re-appends `.d.ts` (see `graph::dts_stem`), so the RAW main path is
        // passed through and resolves to its declaration — no path rewrite here.
        // Only synthesized when no explicit `types` exists; an authoritative field
        // must not be shadowed by a default that may not exist.
        let colocated: Vec<String> = out
            .iter()
            .filter(|e| e.kind == EntryKind::Main)
            .map(|e| e.path.clone())
            .collect();
        type_targets.extend(colocated);
        type_targets.push("index.d.ts".to_string());
    }
    for p in &type_targets {
        push(p, EntryKind::Types, &mut out, &mut seen);
    }

    out
}

/// Recursively gather every `types`/`typings` condition target inside an
/// `exports` subtree (the type surface of `.` and every subpath).
fn collect_export_types(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            for (k, child) in map {
                if (k == "types" || k == "typings")
                    && let Some(s) = child.as_str()
                {
                    out.push(s.to_string());
                } else {
                    collect_export_types(child, out);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_export_types(child, out);
            }
        }
        _ => {}
    }
}

/// Recursively collect every relative-path leaf of an `exports` subtree, carrying
/// the surface `kind` down. Skips `types`/`typings` (they point at `.d.ts`).
fn walk_exports(
    node: &Value,
    kind: EntryKind,
    out: &mut Vec<Entry>,
    seen: &mut BTreeSet<(u8, String)>,
    push: &mut impl FnMut(&str, EntryKind, &mut Vec<Entry>, &mut BTreeSet<(u8, String)>),
) {
    match node {
        Value::String(s) => push(s, kind, out, seen),
        Value::Object(map) => {
            for (k, child) in map {
                if k == "types" || k == "typings" {
                    continue;
                }
                walk_exports(child, kind, out, seen, push);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                walk_exports(child, kind, out, seen, push);
            }
        }
        _ => {}
    }
}

/// Strip a leading `./` and collapse a leading `/`; entry paths are relative to
/// the package root.
fn normalize_rel(p: &str) -> String {
    p.trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Whether a `main`/`module`/`bin`/`exports` target should SEED the reachable
/// walk. Admits a recognized JS file (parsed directly) OR an EXTENSIONLESS /
/// directory-style target — Node resolves `"./dist/index"` → `./dist/index.js`
/// and `"./dist"` → `./dist/index.js` at runtime, and the graph walk's
/// `resolve_entry` runs that SAME ladder, so on-disk resolution is deferred to
/// it (a path that resolves to nothing is simply never analyzed — no false
/// entry). A non-JS asset/type extension (`.json`/`.node`/`.wasm`/`.css`, a
/// `.d.ts` stub) carries no analyzable imports, so it is dropped here and never
/// costs a resolver probe. The `is_js_like`-only gate this replaced dropped
/// extensionless mains outright → `files_analyzed: 0` → every phantom missed.
fn is_entry_candidate(path: &str) -> bool {
    is_js_like(path) || is_sfc_like(path) || is_dts_like(path) || extension(path).is_none()
}

/// A TypeScript declaration file (`.d.ts`/`.d.mts`/`.d.cts`) — the type surface a
/// `Types` entry seeds and the file class the graph walk resolves for that entry.
pub(crate) fn is_dts_like(path: &str) -> bool {
    path.ends_with(".d.ts") || path.ends_with(".d.mts") || path.ends_with(".d.cts")
}

/// A JS-like runtime file (extension we can parse for imports). Excludes `.json`,
/// `.node`, `.wasm`, `.css`, and `.d.ts` type stubs.
pub(crate) fn is_js_like(path: &str) -> bool {
    if path.ends_with(".d.ts") || path.ends_with(".d.mts") || path.ends_with(".d.cts") {
        return false;
    }
    matches!(
        extension(path),
        Some("js" | "cjs" | "mjs" | "jsx" | "ts" | "tsx" | "mts" | "cts")
    )
}

/// A single-file component (Astro/Vue/Svelte) whose imports live in a
/// frontmatter / `<script>` block. Under the global virtual store a backend it
/// imports there — even for TYPES ONLY — still needs project-local
/// materialization: the package's realpath escapes into the shared store, so a
/// type-checker's upward `node_modules` walk can't reach the hoisted backend
/// otherwise (nub#450). The graph walk resolves these and `extract` reads their
/// script region.
pub(crate) fn is_sfc_like(path: &str) -> bool {
    matches!(extension(path), Some("astro" | "vue" | "svelte"))
}

fn extension(path: &str) -> Option<&str> {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map(|(_, e)| e)
}

/// Directory names that ship in tarballs but are never a consumer's import
/// target. Deep-path seeding is speculative — "Node COULD resolve this" — so it
/// is bounded to the directories where a deep import is plausible. `node_modules`
/// is the load-bearing one: a bundled dep's own imports are the BUNDLED
/// package's, and attributing them to the outer package is a false positive by
/// construction.
const NON_SURFACE_DIRS: [&str; 24] = [
    "node_modules",
    "test",
    "tests",
    "__tests__",
    "__test__",
    "spec",
    "__specs__",
    "__mocks__",
    "mocks",
    "fixture",
    "fixtures",
    "__fixtures__",
    "example",
    "examples",
    "demo",
    "demos",
    "benchmark",
    "benchmarks",
    "bench",
    "script",
    "scripts",
    "coverage",
    "doc",
    "docs",
];

/// File names that are a dev script by overwhelming convention, at any depth. A
/// narrower list than [`NON_SURFACE_DIRS`] on purpose: `mocks.js` or `fixtures.js`
/// can plausibly be a real module, `bench.js` cannot.
const DEV_SCRIPT_STEMS: [&str; 8] = [
    "bench",
    "benchmark",
    "benchmarks",
    "test",
    "tests",
    "spec",
    "example",
    "examples",
];

/// Whether a published file may SPECULATIVELY seed the walk as a legacy deep-path
/// entry (`require('<pkg>/lib/integration/react')`), for a package with no
/// `exports` map. Admits JS-like files only, outside the non-surface directories
/// above, excluding dotfiles/dot-dirs (`.eslintrc.js`, `.github/`), `*.test.*` /
/// `*.spec.*` siblings, and build-tool configs (`rollup.config.js`, `gulpfile.js`)
/// — all of which ship routinely and import devDependencies no consumer ever
/// resolves. `.d.ts` is excluded by `is_js_like`; the type surface has its own
/// seeding.
pub(crate) fn is_deep_path_candidate(rel: &str) -> bool {
    if !is_js_like(rel) {
        return false;
    }
    let mut segments = rel.split('/').peekable();
    while let Some(seg) = segments.next() {
        let lower = seg.to_ascii_lowercase();
        if lower.starts_with('.') {
            return false;
        }
        if segments.peek().is_some() {
            if NON_SURFACE_DIRS.contains(&lower.as_str()) {
                return false;
            }
            continue;
        }
        // Last segment: the file name.
        let stem = lower.rsplit_once('.').map_or(lower.as_str(), |(s, _)| s);
        if stem.ends_with(".test")
            || stem.ends_with(".spec")
            // `@aws-crypto/sha256-js` compiles `knownHashes.fixture.ts` into its
            // published `build/`, where it imports an `@aws-sdk` package the
            // manifest declares nowhere — a shipped test fixture, not a surface.
            || stem.ends_with(".fixture")
            || stem.ends_with("-test")
            || stem.ends_with("_test")
            || stem.ends_with(".config")
            || stem.ends_with(".conf")
            || matches!(stem, "gulpfile" | "gruntfile" | "makefile")
            // A file whose whole name is a dev-script convention, wherever it
            // sits. asynckit ships a root `bench.js` requiring `async` and
            // `benchmark` (neither declared, not even as devDeps) purely because
            // it has no `files` whitelist — importable in principle, imported by
            // nobody. Excluding one here can only lose a SPECULATIVE root: a file
            // the published surface actually references is reached in the
            // authoritative phase, before deep-path seeding runs.
            || DEV_SCRIPT_STEMS.contains(&stem)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    #[test]
    fn splits_optional_peers_out_of_required_and_excludes_dev() {
        let raw = br#"{
            "name": "pkg",
            "dependencies": { "a": "1" },
            "devDependencies": { "jest": "1" },
            "peerDependencies": { "react": "*", "zod": "*" },
            "peerDependenciesMeta": { "zod": { "optional": true } }
        }"#;
        let m = Manifest::parse(raw).unwrap();
        assert!(m.deps.contains("a"));
        // devDependencies are NOT in any resolvable set → phantom-eligible.
        assert!(!m.deps.contains("jest") && !m.required_peers.contains("jest"));
        assert!(m.required_peers.contains("react"));
        assert!(m.optional_peers.contains("zod")); // optional peer moved out of required
        assert!(!m.required_peers.contains("zod"));
    }

    #[test]
    fn has_exports_gates_deep_path_seeding_and_candidates_exclude_non_surface_files() {
        // `has_exports` is what decides whether legacy deep-path roots are sound:
        // an `exports` map is Node's encapsulation boundary, so an unexported file
        // genuinely cannot be imported and must never seed the walk.
        assert!(
            !Manifest::parse(br#"{"name":"p","main":"lib/index.js"}"#)
                .unwrap()
                .has_exports
        );
        assert!(
            Manifest::parse(br#"{"name":"p","exports":{".":"./index.js"}}"#)
                .unwrap()
                .has_exports
        );
        assert!(
            Manifest::parse(br#"{"name":"p","exports":null}"#)
                .unwrap()
                .has_exports,
            "`exports: null` exports nothing — still an encapsulation boundary"
        );

        let cand = super::is_deep_path_candidate;
        // The redux-persist@6.0.0 miss: a published file no entry references.
        assert!(cand("lib/integration/react.js"));
        assert!(cand("es/index.mjs") && cand("src/persistReducer.js"));
        // A dev-script FILE name, wherever it sits — asynckit's root `bench.js`.
        assert!(!cand("bench.js") && !cand("test.js") && !cand("example.js"));
        // Non-surface directories that ship in real tarballs.
        for p in [
            "test/index.js",
            "tests/a.js",
            "__tests__/a.js",
            "__mocks__/fs.js",
            "example/app.js",
            "examples/app.js",
            "benchmark/run.js",
            "bench/run.js",
            "scripts/build.js",
            "coverage/lcov.js",
            "node_modules/dep/index.js",
        ] {
            assert!(!cand(p), "{p} must not seed the walk");
        }
        // Dotfiles/dot-dirs, test siblings, and build-tool configs.
        for p in [
            ".eslintrc.js",
            ".github/workflow.js",
            "lib/thing.test.js",
            "lib/thing.spec.js",
            "build/knownHashes.fixture.js",
            "docs/example.js",
            "lib/thing-test.js",
            "rollup.config.js",
            "karma.conf.js",
            "gulpfile.js",
        ] {
            assert!(!cand(p), "{p} must not seed the walk");
        }
        // A `.d.ts` has its own Types surface; non-JS carries no imports.
        assert!(!cand("index.d.ts") && !cand("data.json") && !cand("README.md"));
        // `config.js` is an ordinary module — only `<tool>.config.js` is excluded.
        assert!(cand("lib/config.js"));
    }

    #[test]
    fn collects_entry_points_from_exports_including_type_surface() {
        let raw = br#"{
            "name": "pkg",
            "exports": {
                ".": { "import": "./dist/index.mjs", "require": "./dist/index.cjs", "types": "./dist/index.d.ts" },
                "./sub": "./dist/sub.js"
            }
        }"#;
        let m = Manifest::parse(raw).unwrap();
        let entry = |p: &str| m.entry_points.iter().find(|e| e.path == p);
        assert_eq!(
            entry("dist/index.mjs").unwrap().kind,
            super::EntryKind::Main
        );
        assert_eq!(
            entry("dist/index.cjs").unwrap().kind,
            super::EntryKind::Main
        );
        // `./sub` is a non-`.` export → the adapter surface.
        assert_eq!(
            entry("dist/sub.js").unwrap().kind,
            super::EntryKind::Subpath
        );
        // The `types` condition IS collected — as a Types entry, the `.d.ts` type
        // surface the peer-type walk seeds (nub#450). An explicit `types` suppresses
        // the `index.d.ts` fallback, so no synthesized default appears.
        assert_eq!(
            entry("dist/index.d.ts").unwrap().kind,
            super::EntryKind::Types
        );
        assert!(!m.entry_points.iter().any(|e| e.path == "index.d.ts"));
    }

    #[test]
    fn synthesizes_default_type_roots_only_without_explicit_types() {
        // No `types` field / `exports` types condition → TS's defaults: each Main
        // entry's colocated `.d.ts` plus the root `index.d.ts`, all as Types
        // entries (nub#450). react-pdf's shape (no types field, real index.d.ts).
        let m = Manifest::parse(br#"{"name":"p","main":"./index.js"}"#).unwrap();
        let types: Vec<&str> = m
            .entry_points
            .iter()
            .filter(|e| e.kind == super::EntryKind::Types)
            .map(|e| e.path.as_str())
            .collect();
        assert!(
            types.contains(&"index.d.ts"),
            "root index.d.ts default: {types:?}"
        );
    }

    #[test]
    fn direct_sfc_export_is_collected_as_entry() {
        // A package publishing an .astro/.vue/.svelte directly (not via a JS
        // re-export) must still be scanned, or its type-only phantoms are missed
        // under GVS (nub#450, codex review P1).
        let raw = br#"{
            "name": "pkg",
            "exports": { "./Icon": "./components/Icon.astro", "./Widget": "./Widget.vue" }
        }"#;
        let m = Manifest::parse(raw).unwrap();
        assert_eq!(
            m.entry_points
                .iter()
                .find(|e| e.path == "components/Icon.astro")
                .unwrap()
                .kind,
            super::EntryKind::Subpath
        );
        assert!(m.entry_points.iter().any(|e| e.path == "Widget.vue"));
    }

    #[test]
    fn keeps_extensionless_and_directory_entries_drops_asset_mains() {
        // The recall bug: an extensionless `main` (Node resolves `./dist/index`
        // → `./dist/index.js`) must survive as a candidate for the graph walk to
        // resolve — not be dropped for lacking a literal JS extension. A
        // directory-style main is the same shape. `.json`/`.node`/`.d.ts` targets
        // carry no analyzable imports and stay filtered.
        let has = |m: &Manifest, p: &str| m.entry_points.iter().any(|e| e.path == p);

        let ext = Manifest::parse(br#"{"name":"p","main":"./dist/index"}"#).unwrap();
        assert!(
            has(&ext, "dist/index"),
            "extensionless main kept as candidate"
        );

        let dir = Manifest::parse(br#"{"name":"p","main":"./dist"}"#).unwrap();
        assert!(has(&dir, "dist"), "directory-style main kept as candidate");

        let json = Manifest::parse(br#"{"name":"p","main":"./data.json"}"#).unwrap();
        assert!(
            !has(&json, "data.json"),
            ".json main dropped (not analyzable)"
        );
        // No RUNTIME entry survived → the entry-less fallback (`index.js`) seeds it.
        // (A synthesized `index.d.ts` Types entry also appears — the type-surface
        // default — so filter to the Main surface here.)
        let mains: Vec<&str> = json
            .entry_points
            .iter()
            .filter(|e| e.kind == super::EntryKind::Main)
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(mains, vec!["index.js"]);

        let native = Manifest::parse(br#"{"name":"p","main":"./addon.node"}"#).unwrap();
        assert!(!has(&native, "addon.node"), ".node main dropped");

        // Extensionless conditional target inside an `exports` map is admitted too,
        // and its `types` condition is collected as the Types surface.
        let exp = Manifest::parse(
            br#"{"name":"p","exports":{".":{"import":"./dist/index","types":"./dist/index.d.ts"}}}"#,
        )
        .unwrap();
        assert!(has(&exp, "dist/index"), "extensionless exports target kept");
        assert_eq!(
            exp.entry_points
                .iter()
                .find(|e| e.path == "dist/index.d.ts")
                .unwrap()
                .kind,
            super::EntryKind::Types
        );
    }
}
