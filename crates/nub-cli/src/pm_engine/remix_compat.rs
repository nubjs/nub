//! Version-gated GVS eject for the Remix 3 prereleases.
//!
//! Remix 3 ships an unbundled asset server (`@remix-run/assets`,
//! `createAssetServer`) that serves the browser's whole import graph — app
//! source AND npm packages — straight from disk. Every served file is resolved
//! to its real path and must sit inside a configured mount, and mounts are
//! defined RELATIVE to the app's `rootDir`; a real path outside them fails the
//! request with `IMPORT_OUTSIDE_MOUNTS`. Under nub's machine-global store the
//! transitive `@remix-run/*` packages resolve into the shared store, so the first
//! page load 500s. Same store-locality class as Next's Turbopack and bare RN's
//! Metro.
//!
//! `@remix-run/assets` 0.7.0 (remix-run/remix#11814) reads `virtualStoreDir` from
//! the nearest `node_modules/.modules.yaml` and mounts that store, which is the
//! file the engine writes on every install. Every stable `remix` 3 release
//! depends on `@remix-run/assets@^1`, so only the `3.0.0` prereleases can carry
//! a pre-0.7 asset server (`3.0.0-rc.1` pins `^0.6.0`); later release candidates
//! picked the fix up, but the prerelease line is closed and is ejected whole
//! rather than split by tag. Earlier `remix` majors are bundler-built and never
//! had the asset server.
//!
//! Defaults are computed before resolution, so the signal is the declared
//! range. Under semver a prerelease satisfies a range only through a comparator
//! naming the same `major.minor.patch` with a prerelease tag, so a range admits a
//! `3.0.0-*` version exactly when it spells one: `^3.0.0` and `>=2` cannot select
//! one, while `3.0.0-rc.5` or `^3.0.0-beta.1` can (directly, or through a lockfile
//! that pinned it). A spec that is not a semver range at all — a dist-tag such as
//! `next`, a `catalog:` or `npm:` spec — is ambiguous and ejects: the worst case
//! is a redundant project-local install, never a broken one.

use std::path::{Path, PathBuf};

use super::expo_compat::declared_direct_ranges;

/// The first `remix` release whose dependency range guarantees an asset server
/// that mounts a global virtual store (`@remix-run/assets` >= 0.7.0). Only
/// prereleases of it predate the fix.
const REMIX_STORE_AWARE_RELEASE: &str = "3.0.0";

/// Whether the workspace at `root` declares a `remix` dependency that can
/// resolve to a prerelease of [`REMIX_STORE_AWARE_RELEASE`] — i.e. GVS must be
/// ejected for it. `false` when no manifest declares `remix` or every declared
/// range excludes those prereleases; `true` when any range admits one OR is not
/// a semver range. Same dependency scope as the aube trigger (dependencies /
/// devDependencies / optionalDependencies; peer excluded), root and workspace
/// members alike. `workspace_members` is the caller's one-shot discovery for
/// `root`.
pub(crate) fn remix_needs_project_local_store(root: &Path, workspace_members: &[PathBuf]) -> bool {
    declared_direct_ranges(root, workspace_members, "remix")
        .iter()
        .any(|range| range_may_select_unaware_prerelease(range))
}

fn range_may_select_unaware_prerelease(range: &str) -> bool {
    let range = range.trim();
    if node_semver::Range::parse(range).is_err() {
        return true;
    }
    let prerelease = format!("{REMIX_STORE_AWARE_RELEASE}-");
    range
        .match_indices(&prerelease)
        .any(|(at, _)| !range[..at].ends_with(|c: char| c.is_ascii_digit() || c == '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("package.json"), manifest).expect("write manifest");
        dir
    }

    /// What `nub_setting_defaults` hands the gate: the discovered members.
    fn ejects(root: &Path) -> bool {
        let members = aube_workspace::find_workspace_packages(root).unwrap_or_default();
        remix_needs_project_local_store(root, &members)
    }

    #[test]
    fn only_ranges_admitting_a_3_0_0_prerelease_eject() {
        for (deps, want) in [
            (r#"{"remix":"3.0.0-rc.5"}"#, true),
            (r#"{"remix":"3.0.0-rc.1"}"#, true),
            (r#"{"remix":"^3.0.0-beta.1"}"#, true),
            (r#"{"remix":">=3.0.0-alpha.0"}"#, true),
            (r#"{"remix":"3.0.0-rc.1 || ^3.1.0"}"#, true),
            (r#"{"remix":"next"}"#, true),
            (r#"{"remix":"catalog:"}"#, true),
            (r#"{"remix":"3.0.0"}"#, false),
            (r#"{"remix":"3.1.0"}"#, false),
            (r#"{"remix":"^3.0.0"}"#, false),
            (r#"{"remix":"~3.1.0"}"#, false),
            (r#"{"remix":">=2"}"#, false),
            (r#"{"remix":"*"}"#, false),
            (r#"{"remix":"13.0.0-rc.1"}"#, false),
            (r#"{"remix":"^2.16.0"}"#, false),
            (r#"{"@remix-run/node":"^2.16.0"}"#, false),
            (r#"{}"#, false),
        ] {
            let dir = project_with(&format!(r#"{{"name":"app","dependencies":{deps}}}"#));
            assert_eq!(ejects(dir.path()), want, "dependencies={deps}");
        }
    }

    #[test]
    fn a_dev_dependency_counts_like_the_aube_trigger() {
        let dir = project_with(r#"{"name":"app","devDependencies":{"remix":"3.0.0-rc.1"}}"#);
        assert!(ejects(dir.path()));
    }

    /// The engine's trigger scan reads every workspace importer, so a member
    /// declaring a `remix` prerelease must eject the whole install even when the root
    /// manifest never mentions it.
    #[test]
    fn a_workspace_member_declaring_a_remix_prerelease_ejects_the_install() {
        let dir = project_with(r#"{"name":"mono","workspaces":["apps/*"]}"#);
        let member = dir.path().join("apps/web");
        std::fs::create_dir_all(&member).expect("member dir");
        std::fs::write(
            member.join("package.json"),
            r#"{"name":"web","dependencies":{"remix":"3.0.0-rc.1"}}"#,
        )
        .expect("member manifest");
        assert!(ejects(dir.path()));
    }
}
