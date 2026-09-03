//! Version-gated GVS eject for Remix 3.
//!
//! Remix 3 ships an unbundled asset server (`@remix-run/assets`,
//! `createAssetServer`) that serves the browser's whole import graph — app
//! source AND npm packages — straight from disk. Every served file is resolved
//! to its real path and must sit inside a configured mount, and mounts are
//! defined RELATIVE to the app's `rootDir` (the defaults are `app` and
//! `node_modules`); a real path outside them fails the request with
//! `IMPORT_OUTSIDE_MOUNTS`. Under nub's machine-global store the transitive
//! `@remix-run/*` packages the browser loads resolve into the shared store, so
//! the first page load 500s. pnpm passes only because its real paths stay under
//! the project's own `node_modules/.pnpm`. There is no Remix-side escape:
//! because mount values must be relative to `rootDir`, the shared store cannot
//! be mounted in. Same store-locality class as Next's Turbopack and bare RN's
//! Metro — the whole install must be project-local.
//!
//! Gated on the declared `remix` major. `remix@3` is the first release that
//! carries the asset server; earlier `remix` majors belong to apps built by
//! `@remix-run/dev` (esbuild, later Vite), which reach the shared store fine, so
//! they keep GVS. Eject-on-ambiguity as for `expo`: a range whose major can't
//! be read pre-resolution (`next`, `*`, a `catalog:` spec) ejects — the worst
//! case is a redundant project-local install, never a broken one. This gate
//! runs in the OPPOSITE direction from Expo's, which changes what a lower-bound
//! comparator means: `>=2` floors at 2 yet selects 3 the moment it is
//! published, so a bare `>`/`>=` range is ambiguous here and ejects, while a
//! caret/tilde/exact range is confined to its major and keeps GVS below 3.

use std::path::Path;

use super::expo_compat::{declared_direct_ranges, major_floor};

/// The `remix` major at and above which the unbundled asset server ships.
const REMIX_ASSET_SERVER_FLOOR: u32 = 3;

/// Whether the workspace at `root` declares a `remix` dependency that can
/// resolve to the asset-server major — i.e. GVS must be ejected for it.
/// `false` when no manifest declares `remix` or every declared range is
/// confined below the floor; `true` when any range reaches the floor OR is
/// ambiguous (unfloorable, or an open lower bound). Same dependency scope as
/// the aube trigger (dependencies / devDependencies / optionalDependencies;
/// peer excluded), root and workspace members alike.
pub(crate) fn remix_needs_project_local_store(root: &Path) -> bool {
    declared_direct_ranges(root, "remix")
        .iter()
        .any(|range| range_may_select_asset_server(range))
}

fn range_may_select_asset_server(range: &str) -> bool {
    if range.trim_start().starts_with('>') {
        return true;
    }
    match major_floor(range) {
        Some(major) => major >= REMIX_ASSET_SERVER_FLOOR,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("package.json"), manifest).expect("write manifest");
        dir
    }

    #[test]
    fn remix_3_and_ambiguous_ranges_eject_while_older_majors_keep_gvs() {
        for (deps, want) in [
            (r#"{"remix":"3.0.0-rc.1"}"#, true),
            (r#"{"remix":"^3.0.0"}"#, true),
            (r#"{"remix":"next"}"#, true),
            (r#"{"remix":">=2.0.0"}"#, true),
            (r#"{"remix":">2"}"#, true),
            (r#"{"remix":"^2.16.0"}"#, false),
            (r#"{"remix":"~2.16.0"}"#, false),
            (r#"{"remix":"1.19.3"}"#, false),
            (r#"{"@remix-run/node":"^2.16.0"}"#, false),
            (r#"{}"#, false),
        ] {
            let dir = project_with(&format!(r#"{{"name":"app","dependencies":{deps}}}"#));
            assert_eq!(
                remix_needs_project_local_store(dir.path()),
                want,
                "dependencies={deps}"
            );
        }
    }

    #[test]
    fn a_dev_dependency_counts_like_the_aube_trigger() {
        let dir = project_with(r#"{"name":"app","devDependencies":{"remix":"^3.0.0"}}"#);
        assert!(remix_needs_project_local_store(dir.path()));
    }

    /// The engine's trigger scan reads every workspace importer, so a member
    /// declaring `remix@3` must eject the whole install even when the root
    /// manifest never mentions it.
    #[test]
    fn a_workspace_member_declaring_remix_3_ejects_the_install() {
        let dir = project_with(r#"{"name":"mono","workspaces":["apps/*"]}"#);
        let member = dir.path().join("apps/web");
        std::fs::create_dir_all(&member).expect("member dir");
        std::fs::write(
            member.join("package.json"),
            r#"{"name":"web","dependencies":{"remix":"^3.0.0"}}"#,
        )
        .expect("member manifest");
        assert!(remix_needs_project_local_store(dir.path()));
    }
}
