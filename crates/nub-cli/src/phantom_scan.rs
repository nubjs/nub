//! Pre-run PHANTOM-dependency scan — the static half of the phantom nudge.
//!
//! A phantom is a package the project's OWN source imports without declaring in
//! its `package.json`, that is only present TRANSITIVELY (hoisted into
//! `node_modules` by a dependency). Such an import resolves today under a
//! flat-hoisting install (npm/yarn) but breaks the moment the tree is installed
//! isolated (nub's / pnpm's default) — so nub warns, naming the file and the fix
//! (declare it), turning a future bare `MODULE_NOT_FOUND` into an actionable
//! nudge. Warn-only: this never changes the exit code (the runtime-enforcement
//! half is deferred).
//!
//! Precision is the whole game — a noisy warning trains users to ignore it — so
//! the scan is conservative by construction. A package is flagged ONLY when
//! EVERY one of these holds:
//!
//! - it is referenced by at least one UNGUARDED occurrence (a require/import in
//!   a `try/catch` or a conditional branch is soft — an intentional optional
//!   load — and never flagged; the guard modeling is `nub_phantom_core::extract`),
//! - it is not a Node builtin, the project's own name (self-import), or a
//!   `@types/*` package (type-only, erased before runtime),
//! - it is not declared in ANY of `dependencies` / `devDependencies` /
//!   `optionalDependencies` / `peerDependencies`,
//! - and it actually RESOLVES in the installed tree to a genuine hoisted
//!   dependency — its real (symlink-resolved) location is inside a
//!   `node_modules` directory. This both confirms the "present transitively"
//!   half of the definition and excludes workspace members (which symlink into
//!   `node_modules` but resolve to first-party source OUTSIDE it).
//!
//! Extraction reuses the SAME oxc 0.132.0 parser nub transpiles with (via
//! `nub-phantom-core`), so what the scan sees matches what nub actually loads.

use std::collections::BTreeMap;
use std::path::Path;

use nub_core::workspace::detect::Project;
use nub_phantom_core::builtins::is_builtin;
use nub_phantom_core::extract::extract;
use nub_phantom_core::specifier::{self, SpecKind};

/// Bound the first-party walk so the once-per-command pre-run path can't blow up
/// on a giant repo: past these limits the scan stops and stays silent
/// (under-warning is cheap; a slow start is not). A phantom import is
/// overwhelmingly in a hand-written source file, which is small.
const MAX_FILES: usize = 4096;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Cap emitted lines so a pathologically phantom-ridden project can't produce a
/// wall of warnings; the rest are summarized.
const MAX_REPORTED: usize = 10;

/// Source extensions nub itself can load/transpile. `.d.ts` is intentionally
/// excluded — it is type-only and carries no runtime imports.
const SOURCE_EXTS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"];

/// Directory names never descended into: third-party trees and generated output
/// (whose imports are bundler-resolved, not the project's declared surface). Any
/// dotfile directory (`.next`, `.git`, `.turbo`, …) is skipped separately.
const SKIP_DIRS: &[&str] = &["node_modules", "dist", "build", "out", "coverage", "target"];

/// Run the scan for `project` and print one warning per phantom. No-op when the
/// check is disabled, or when nothing is installed (a phantom is by definition
/// transitively PRESENT — with no tree there is nothing to diagnose).
pub(crate) fn scan_and_warn(project: &Project) {
    if !policy_enabled(&project.root) {
        return;
    }
    if !tree_installed(&project.root, project.workspace_root.as_deref()) {
        return;
    }

    let declared = declared_surface(&project.manifest);
    let self_name = project
        .manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let candidates = collect_hard_candidates(&project.root, &declared, self_name);
    // Resolve-gate LAST (it touches the filesystem): keep only undeclared hard
    // imports that are genuinely present as a hoisted dependency.
    let mut phantoms: Vec<(String, String)> = candidates
        .into_iter()
        .filter(|(name, _)| resolves_as_hoisted_dep(&project.root, name))
        .collect();
    phantoms.sort();

    report(&phantoms);
}

/// Whether an installed tree exists to diagnose against. A phantom is
/// transitively PRESENT, so with nothing installed there is nothing to find (and
/// no reason to pay for the source walk). In a hoisted workspace a member has no
/// local `node_modules` — its deps live at the workspace root — so a tree at
/// EITHER the project root or the workspace root counts.
fn tree_installed(root: &Path, workspace_root: Option<&Path>) -> bool {
    root.join("node_modules").is_dir()
        || workspace_root.is_some_and(|w| w.join("node_modules").is_dir())
}

fn report(phantoms: &[(String, String)]) {
    for (pkg, file) in phantoms.iter().take(MAX_REPORTED) {
        eprintln!(
            "nub: {file} imports `{pkg}`, which isn't in package.json. Run `nub add {pkg}` (WARN_PHANTOM_DEP)."
        );
    }
    let extra = phantoms.len().saturating_sub(MAX_REPORTED);
    if extra > 0 {
        eprintln!("nub: …and {extra} more undeclared dependencies (WARN_PHANTOM_DEP).");
    }
}

/// Every name that would make an import non-phantom if referenced: the union of
/// the four dependency maps. Names only; ranges are irrelevant to declaredness.
fn declared_surface(manifest: &serde_json::Value) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    for key in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if let Some(map) = manifest.get(key).and_then(|v| v.as_object()) {
            set.extend(map.keys().cloned());
        }
    }
    set
}

/// Walk first-party source and aggregate the undeclared, non-builtin, non-self
/// package names that have at least one HARD (unguarded) reference, each mapped
/// to an example importing file (relative to `root`) for the warning.
///
/// Soft-ness aggregates across the whole project: a package required under a
/// `try/catch` in one file and imported unguarded in another is hard (a real
/// dependency). A package that is ONLY ever soft-loaded is an intentional
/// optional and never surfaces here.
fn collect_hard_candidates(
    root: &Path,
    declared: &std::collections::BTreeSet<String>,
    self_name: &str,
) -> BTreeMap<String, String> {
    struct Agg {
        hard: bool,
        example: String,
    }
    let mut by_pkg: BTreeMap<String, Agg> = BTreeMap::new();
    let mut budget = MAX_FILES;

    walk_source(root, &mut budget, &mut |path, source| {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        for occ in extract(&path.to_string_lossy(), source) {
            let SpecKind::Bare(name) = specifier::classify(&occ.spec) else {
                continue;
            };
            // Constant per-name exclusions — drop before aggregating.
            if is_builtin(&name)
                || name == self_name
                || name.starts_with("@types/")
                || declared.contains(&name)
            {
                continue;
            }
            let e = by_pkg.entry(name).or_insert(Agg {
                hard: false,
                example: rel.clone(),
            });
            if !occ.soft {
                // Prefer a hard occurrence's file as the cited example.
                if !e.hard {
                    e.example = rel.clone();
                }
                e.hard = true;
            }
        }
    });

    by_pkg
        .into_iter()
        .filter(|(_, agg)| agg.hard)
        .map(|(name, agg)| (name, agg.example))
        .collect()
}

/// Recursively walk `dir`, invoking `visit(path, source)` for each readable
/// source file, decrementing `budget` per file and stopping the whole walk when
/// it hits zero. Skips third-party/output dirs and dotfile dirs; skips files
/// larger than [`MAX_FILE_BYTES`] (generated/minified, not hand-written source).
fn walk_source(dir: &Path, budget: &mut usize, visit: &mut dyn FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Do not follow symlinks — they point into node_modules or out of the
        // project; first-party source is real files.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            // A nested `package.json` marks a distinct package (a workspace
            // member, a nested tool). Its imports are declared in ITS OWN
            // manifest, not the root's — checking them against the root manifest
            // would false-flag a member's properly-declared, root-hoisted dep. So
            // the scan is scoped to ONE package: nub checks a member's phantoms
            // when it runs inside that member (where `detect_project` roots there).
            if path.join("package.json").is_file() {
                continue;
            }
            walk_source(&path, budget, visit);
        } else if file_type.is_file() && is_source_file(&path) {
            if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
                continue;
            }
            if let Ok(source) = std::fs::read_to_string(&path) {
                *budget -= 1;
                visit(&path, &source);
            }
        }
    }
}

fn is_source_file(path: &Path) -> bool {
    // `.d.ts` is type-only — exclude even though its extension is `ts`.
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && name.ends_with(".d.ts")
    {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SOURCE_EXTS.contains(&ext))
}

/// True iff `name` resolves in the `node_modules` chain from `root` AND its real
/// (symlink-resolved) location lives UNDER the specific `node_modules` it was
/// found in — i.e. it is a genuine transitively-present dependency, not a
/// workspace member.
///
/// A workspace member is symlinked into `node_modules` but resolves to
/// first-party source OUTSIDE it, so `starts_with` is false and it is never a
/// phantom. pnpm's `<nm>/.pnpm/<name>@<v>/node_modules/<name>` target still lives
/// under the same `node_modules`, so a real isolated-store dep still counts. The
/// check is anchored to THIS `node_modules` (not "a `node_modules` component
/// anywhere in the absolute path"), so a project that itself lives under some
/// ancestor `node_modules` doesn't defeat the workspace-member exclusion.
fn resolves_as_hoisted_dep(root: &Path, name: &str) -> bool {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let nm = d.join("node_modules");
        let pkg = nm.join(name).join("package.json");
        if pkg.is_file() {
            let real = std::fs::canonicalize(&pkg).unwrap_or_else(|_| pkg.clone());
            let nm_real = std::fs::canonicalize(&nm).unwrap_or(nm);
            return real.starts_with(&nm_real);
        }
        dir = d.parent();
    }
    false
}

/// Whether the phantom scan runs. Off via the neutral `phantom-check` npmrc key
/// (`off`/`false`/`0`/`no`) or the `NUB_PHANTOM_CHECK` env override; otherwise
/// on (warn). Error/enforce is deferred — the scan is warn-only for now.
fn policy_enabled(project_root: &Path) -> bool {
    let off = |v: &str| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "0" | "no" | "none" | "skip"
        )
    };
    if let Ok(v) = std::env::var("NUB_PHANTOM_CHECK") {
        return !off(&v);
    }
    if let Some(v) = crate::pm_engine::unsupported_config::npmrc_scalar_value(
        project_root,
        "phantom-check",
        true,
    ) {
        return !off(&v);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-phantom-scan-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Install a fake hoisted dependency `<name>` at `<root>/node_modules/<name>`.
    fn install_dep(root: &Path, name: &str) {
        let d = root.join("node_modules").join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
    }

    fn run(root: &Path, manifest: &str) -> Vec<(String, String)> {
        let manifest: serde_json::Value = serde_json::from_str(manifest).unwrap();
        let declared = declared_surface(&manifest);
        let self_name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let candidates = collect_hard_candidates(root, &declared, self_name);
        let mut out: Vec<(String, String)> = candidates
            .into_iter()
            .filter(|(name, _)| resolves_as_hoisted_dep(root, name))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn flags_undeclared_transitively_present_import_only() {
        let root = tmpdir("hard");
        fs::write(
            root.join("index.js"),
            r#"
            const declared = require('declared-dep');
            const ghost = require('phantom-dep');
            const fs = require('node:fs');
            let opt; try { opt = require('soft-dep'); } catch {}
            "#,
        )
        .unwrap();
        // All four resolve in the tree; only `phantom-dep` is an undeclared HARD
        // import → the sole phantom.
        install_dep(&root, "declared-dep");
        install_dep(&root, "phantom-dep");
        install_dep(&root, "soft-dep");
        let got = run(
            &root,
            r#"{"name":"app","dependencies":{"declared-dep":"1"}}"#,
        );
        assert_eq!(
            got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["phantom-dep"]
        );
        assert_eq!(got[0].1, "index.js");
    }

    #[test]
    fn silent_when_undeclared_import_does_not_resolve() {
        // Undeclared AND absent from node_modules → not a phantom (a genuinely
        // missing dep / typo is out of scope; only transitively-present is).
        let root = tmpdir("absent");
        fs::write(root.join("index.js"), "require('not-installed');").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        assert!(run(&root, r#"{"name":"app"}"#).is_empty());
    }

    #[test]
    fn devdeps_optional_and_peer_declarations_are_not_phantoms() {
        let root = tmpdir("declared");
        fs::write(
            root.join("index.ts"),
            r#"
            import 'a-dev';
            import 'an-optional';
            import 'a-peer';
            "#,
        )
        .unwrap();
        install_dep(&root, "a-dev");
        install_dep(&root, "an-optional");
        install_dep(&root, "a-peer");
        let got = run(
            &root,
            r#"{"name":"app","devDependencies":{"a-dev":"1"},
                "optionalDependencies":{"an-optional":"1"},
                "peerDependencies":{"a-peer":"1"}}"#,
        );
        assert!(
            got.is_empty(),
            "declared in any map → not a phantom: {got:?}"
        );
    }

    #[test]
    fn type_only_import_and_types_package_are_not_phantoms() {
        let root = tmpdir("types");
        fs::write(
            root.join("index.ts"),
            r#"
            import type { T } from 'type-only-pkg';
            import '@types/node';
            "#,
        )
        .unwrap();
        install_dep(&root, "type-only-pkg");
        install_dep(&root, "@types/node");
        assert!(run(&root, r#"{"name":"app"}"#).is_empty());
    }

    #[test]
    fn node_modules_and_output_dirs_are_not_scanned() {
        let root = tmpdir("skip");
        install_dep(&root, "phantom-dep");
        // Imports live ONLY in third-party / generated trees — must be ignored.
        fs::write(
            root.join("node_modules").join("phantom-dep").join("lib.js"),
            "require('phantom-dep');",
        )
        .unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(
            root.join("dist").join("bundle.js"),
            "require('phantom-dep');",
        )
        .unwrap();
        assert!(run(&root, r#"{"name":"app"}"#).is_empty());
    }

    #[test]
    fn tree_installed_accepts_local_or_workspace_root_node_modules() {
        let root = tmpdir("tree");
        let member = root.join("packages").join("m");
        fs::create_dir_all(&member).unwrap();
        // No tree anywhere → false.
        assert!(!tree_installed(&member, Some(&root)));
        // Hoisted workspace: only the workspace root has node_modules → true (the
        // member's deps live there, so the scan must still run).
        fs::create_dir_all(root.join("node_modules")).unwrap();
        assert!(tree_installed(&member, Some(&root)));
        // A standalone project with a local tree → true.
        fs::create_dir_all(member.join("node_modules")).unwrap();
        assert!(tree_installed(&member, None));
    }

    #[test]
    fn hoisted_workspace_root_run_does_not_flag_member_deps() {
        // The P1 regression: running from a workspace ROOT, the scan must not
        // walk a member's source and flag a dep the MEMBER declares (but the root
        // doesn't) just because npm/Yarn hoisted it to the root node_modules. The
        // member's own package.json marks a package boundary the walk stops at.
        let root = tmpdir("ws-root");
        let member = root.join("packages").join("foo");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(
            member.join("package.json"),
            r#"{"name":"@app/foo","dependencies":{"member-dep":"1"}}"#,
        )
        .unwrap();
        fs::write(member.join("src").join("i.ts"), "import 'member-dep';").unwrap();
        // member-dep is declared by the member but hoisted to the ROOT tree and
        // absent from the root manifest — the false-positive shape.
        install_dep(&root, "member-dep");
        let got = run(
            &root,
            r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
        );
        assert!(
            got.is_empty(),
            "member's declared dep must not be flagged at the root: {got:?}"
        );
    }

    #[test]
    fn scoped_package_phantom_is_flagged() {
        let root = tmpdir("scoped");
        fs::write(root.join("index.js"), "require('@scope/ghost/sub');").unwrap();
        install_dep(&root, "@scope/ghost");
        let got = run(&root, r#"{"name":"app"}"#);
        assert_eq!(
            got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["@scope/ghost"]
        );
    }

    #[test]
    fn soft_in_one_file_hard_in_another_aggregates_to_hard() {
        // A package guarded in one file but imported unguarded in another is a
        // real dependency (hard wins across the whole project).
        let root = tmpdir("agg");
        fs::write(
            root.join("a.cjs"),
            "let x; try { x = require('shared-ghost'); } catch {}",
        )
        .unwrap();
        fs::write(root.join("b.cjs"), "const y = require('shared-ghost');").unwrap();
        install_dep(&root, "shared-ghost");
        let got = run(&root, r#"{"name":"app"}"#);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "shared-ghost");
        assert_eq!(got[0].1, "b.cjs", "example cites the hard-occurrence file");
    }

    #[test]
    fn workspace_member_symlink_is_not_a_phantom() {
        // A workspace member symlinked into node_modules resolves OUTSIDE any
        // node_modules → excluded. (Unix-only symlink; Windows path skips.)
        #[cfg(unix)]
        {
            let root = tmpdir("ws");
            let member = root.join("packages").join("shared");
            fs::create_dir_all(&member).unwrap();
            fs::write(
                member.join("package.json"),
                r#"{"name":"@app/shared","version":"1.0.0"}"#,
            )
            .unwrap();
            fs::create_dir_all(root.join("node_modules").join("@app")).unwrap();
            std::os::unix::fs::symlink(
                &member,
                root.join("node_modules").join("@app").join("shared"),
            )
            .unwrap();
            fs::write(root.join("index.js"), "require('@app/shared');").unwrap();
            assert!(run(&root, r#"{"name":"app"}"#).is_empty());
        }
    }
}
