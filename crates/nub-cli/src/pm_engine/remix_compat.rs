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
//! case is a redundant project-local install, never a broken one.

use std::path::Path;

use super::expo_compat::{declared_direct_range, major_floor};

/// The `remix` major at and above which the unbundled asset server ships.
const REMIX_ASSET_SERVER_FLOOR: u32 = 3;

/// Whether the project at `root` declares a `remix` dependency whose major is
/// at or above the asset-server floor — i.e. GVS must be ejected for it.
/// `false` when `remix` is not a direct dependency or its declared major is
/// below the floor; `true` at/above it OR when the range can't be floor-parsed
/// (eject-on-ambiguity). Same dependency scope as the aube trigger
/// (dependencies / devDependencies / optionalDependencies; peer excluded).
pub(crate) fn remix_needs_project_local_store(root: &Path) -> bool {
    let Some(range) = declared_direct_range(root, "remix") else {
        return false;
    };
    match major_floor(&range) {
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
            (r#"{"remix":"^2.16.0"}"#, false),
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
}
