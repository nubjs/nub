//! Which Node engine a nub install built `node_modules` for (issue #548).
//!
//! A native addon is compiled against exactly one `NODE_MODULE_VERSION`, so a
//! tree whose lifecycle scripts ran under Node 22 dies with
//! `ERR_DLOPEN_FAILED … NODE_MODULE_VERSION 127 … requires 147` the moment the
//! project's Node moves to 26. The pre-run freshness walk cannot see that: the
//! wrong-ABI package is present and its version satisfies the manifest, so the
//! walk is engine-blind by construction. Nub therefore records, beside the tree
//! it built, WHICH engine built it, and the run path compares that stamp
//! against the engine it is about to run under (see
//! [`verify_deps`](crate::verify_deps)).
//!
//! Only a successful nub install writes the stamp, which is what scopes the
//! auto-repair: nub redoes nub's OWN work, and never reshapes a tree
//! npm/pnpm/yarn owns. The recorded value is the engine's own artifact-keying
//! axis (`<os>-<arch>-node<major>` — what the virtual store and the
//! side-effects cache already segregate builds on), so the stamp cannot
//! disagree with how the artifacts were actually keyed.

use std::path::{Path, PathBuf};

use nub_core::workspace::detect::detect_project;

const STAMP_FILE: &str = ".nub-engine";

/// The directory a nub install keys its tree at: the workspace root when the
/// project is a member, else the project root. Same rule
/// `pm_engine::apply_lifecycle_augmentation` anchors Node discovery on — one
/// workspace materializes ONE tree, so a member's own `.nvmrc` must not name a
/// different engine than the root the install actually wrote.
pub(crate) fn anchor(cwd: &Path) -> Option<PathBuf> {
    detect_project(cwd).map(|p| p.workspace_root.unwrap_or(p.root))
}

fn stamp_path(anchor: &Path) -> PathBuf {
    anchor.join("node_modules").join(STAMP_FILE)
}

/// `<os>-<arch>-node<major>` for a resolved Node version.
pub(crate) fn engine_name(node_version: &str) -> String {
    aube_lockfile::graph_hash::engine_name_default(node_version).0
}

/// The engine a nub install last built `anchor`'s tree for. `None` when nub
/// never installed it, or installed it before this stamp existed — both
/// degrade to the pre-#548 behavior (no repair), never to a wrong verdict.
pub(crate) fn recorded(anchor: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(stamp_path(anchor)).ok()?;
    let engine = raw.trim();
    (!engine.is_empty()).then(|| engine.to_string())
}

/// Record the engine after a tree-materializing verb. Takes the version from
/// the engine context, where `apply_lifecycle_augmentation` published the SAME
/// value the engine keyed its build artifacts with — re-discovering here could
/// name a different Node and stamp a lie. Best-effort: an install that already
/// succeeded must not fail on an unwritable stamp.
pub(crate) fn record(cwd: &Path, code: i32) {
    record_for(
        cwd,
        code,
        aube_util::engine_context().runtime_node_version.as_deref(),
    );
}

/// The gated write, split from its process-global engine-context read so the
/// success gate is unit-testable without touching that global. A non-zero exit
/// records nothing — a failed install must not stamp the tree as built for this
/// engine — and an unresolved version has nothing to record.
fn record_for(cwd: &Path, code: i32, version: Option<&str>) {
    if code != 0 {
        return;
    }
    let Some(version) = version else {
        return;
    };
    let Some(anchor) = anchor(cwd) else {
        return;
    };
    write_stamp(&anchor, &engine_name(version));
}

/// Write the stamp INTO an existing tree only. A verb that installed nothing
/// (no `node_modules` on disk) has no tree to describe, and conjuring the
/// directory just to hold a stamp would make the next run believe a nub
/// install had happened.
fn write_stamp(anchor: &Path, engine: &str) {
    let path = stamp_path(anchor);
    if path.parent().is_some_and(Path::is_dir) {
        let _ = std::fs::write(path, engine);
    }
}

/// Human phrasing for an engine change: Node majors when both names carry one
/// (the ABI axis users actually move along), the raw engine names otherwise so
/// a platform-only change still reads truthfully.
pub(crate) fn describe(recorded: &str, current: &str) -> (String, String) {
    match (node_major(recorded), node_major(current)) {
        (Some(was), Some(now)) if was != now => (format!("Node {was}"), format!("Node {now}")),
        _ => (recorded.to_string(), current.to_string()),
    }
}

fn node_major(engine: &str) -> Option<&str> {
    let major = engine.rsplit_once("-node")?.1;
    (!major.is_empty() && major.bytes().all(|b| b.is_ascii_digit())).then_some(major)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_round_trips_only_through_an_installed_tree() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path();

        // No node_modules: there is no tree to describe, so write_stamp must not
        // create one just to hold the marker — a later run would then believe a
        // nub install had happened here.
        write_stamp(anchor, "darwin-arm64-node22");
        assert_eq!(recorded(anchor), None);
        assert!(!anchor.join("node_modules").exists());

        // With a tree present, the marker round-trips; a blank marker reads back
        // as "no record" rather than an empty engine name.
        std::fs::create_dir(anchor.join("node_modules")).unwrap();
        write_stamp(anchor, "darwin-arm64-node22");
        assert_eq!(recorded(anchor), Some("darwin-arm64-node22".to_string()));
        std::fs::write(anchor.join("node_modules/.nub-engine"), "  \n").unwrap();
        assert_eq!(recorded(anchor), None, "a blank marker is not a record");
    }

    #[test]
    fn only_a_successful_install_records_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path();
        std::fs::write(anchor.join("package.json"), "{\"name\":\"app\"}").unwrap();
        std::fs::create_dir(anchor.join("node_modules")).unwrap();

        // A failed verb must not claim the tree is built for this engine…
        record_for(anchor, 1, Some("22.15.0"));
        assert_eq!(recorded(anchor), None);
        // …nor may an install with no resolved Node version stamp a guess.
        record_for(anchor, 0, None);
        assert_eq!(recorded(anchor), None);
        // A success with a known engine records it.
        record_for(anchor, 0, Some("22.15.0"));
        assert_eq!(recorded(anchor), Some(engine_name("22.15.0")));
    }

    #[test]
    fn engine_name_tracks_the_node_major_only() {
        assert_eq!(engine_name("22.15.0"), engine_name("22.16.3"));
        assert_ne!(engine_name("22.15.0"), engine_name("26.5.0"));
        assert!(engine_name("26.5.0").ends_with("-node26"));
    }

    #[test]
    fn describe_names_majors_when_both_engines_carry_one() {
        assert_eq!(
            describe("darwin-arm64-node22", "darwin-arm64-node26"),
            ("Node 22".to_string(), "Node 26".to_string())
        );
        // Platform-only move: majors alone would read "Node 26 → Node 26".
        assert_eq!(
            describe("darwin-x64-node26", "darwin-arm64-node26"),
            (
                "darwin-x64-node26".to_string(),
                "darwin-arm64-node26".to_string()
            )
        );
        // A stamp written when no Node version resolved carries no major.
        assert_eq!(
            describe("darwin-arm64", "darwin-arm64-node26"),
            (
                "darwin-arm64".to_string(),
                "darwin-arm64-node26".to_string()
            )
        );
    }
}
