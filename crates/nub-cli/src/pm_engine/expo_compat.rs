//! Version-gated GVS eject for Expo.
//!
//! Expo gained global-virtual-store compatibility in **SDK 56** via its
//! "On-demand Filesystem" (`@expo/cli` 56.0.0 / the `@expo/metro-file-map`
//! fork), which lets Metro follow symlinks out of `watchFolders` into a
//! machine-global store. Below SDK 56 Expo uses the eager `metro-file-map`
//! realpath crawl, which cannot reach nub's machine-global store — the same
//! store-locality break as bare `react-native`, so those projects must fall
//! back to a project-local store.
//!
//! `react-native` ejects unconditionally (no version gates it — On-demand FS is
//! an Expo-CLI feature, absent from upstream Metro at every RN version). `expo`
//! is version-conditional: eject below the floor, leave GVS on at/above it. The
//! SDK number tracks the top-level `expo` major exactly (SDK 56 → `expo@56.x`),
//! which is Expo's own doctor predicate (`semver.satisfies(sdkVersion,
//! '>=56.0.0')`), so the declared `expo` range is the signal.
//!
//! Defaults are computed before resolution, so only the DECLARED range is
//! available (not a resolved version) — read from the root manifest and every
//! workspace member's, because the engine's own trigger scan covers every
//! importer and a member-declared framework must reach the same verdict. A
//! range whose major can't be read pre-resolution (`*`, `latest`, a compound
//! `>=50 <60`, a `catalog:`/`workspace:` protocol spec) is treated as
//! below-floor and EJECTS — the safe direction, matching `react-native`: the
//! worst case is a redundant project-local install, never a broken one. A
//! lower-bound comparator (`>=50`) floors correctly in THIS direction: it can
//! only select at or above its major, so a floor under 56 ejects and a floor at
//! or above it keeps GVS. The residual case the version
//! gate can't see is a 56+ project that disables On-demand FS via
//! `experiments.onDemandFilesystem: false`, which lives in `app.json`/
//! `app.config.*` rather than `package.json`; that is an accepted, documented
//! edge case.

use std::path::{Path, PathBuf};

/// The `expo` major at and above which the On-demand Filesystem makes a project
/// GVS-compatible (Expo SDK 56).
const EXPO_GVS_FLOOR: u32 = 56;

/// Whether the workspace at `root` declares an `expo` dependency whose SDK is
/// below the GVS floor — i.e. GVS must be ejected for it. `false` when no
/// manifest declares `expo` (not an Expo project) or every declared major is
/// `>= EXPO_GVS_FLOOR`; `true` when any is below the floor OR can't be
/// floor-parsed (eject-on-ambiguity). Matches the aube trigger's dependency
/// scope (dependencies / devDependencies / optionalDependencies; peer excluded).
pub(crate) fn expo_below_gvs_floor(root: &Path, workspace_members: &[PathBuf]) -> bool {
    declared_direct_ranges(root, workspace_members, "expo")
        .iter()
        .any(|range| match major_floor(range) {
            Some(major) => major < EXPO_GVS_FLOOR,
            None => true,
        })
}

/// Every declared range of direct dependency `name` across the root manifest
/// and each workspace member's, in that order. `workspace_members` is the
/// caller's one-shot discovery for `root` (`nub_setting_defaults` runs it once
/// and shares it with the injected-deps check); the engine's own trigger scan
/// checks every importer, so a version gate that read only the root would let
/// a member-declared framework keep GVS. Uses the shared mtime-cached parse so
/// the extra reads are free. Shared with the other version-gated ejects
/// ([`super::remix_compat`]).
pub(super) fn declared_direct_ranges(
    root: &Path,
    workspace_members: &[PathBuf],
    name: &str,
) -> Vec<String> {
    std::iter::once(root)
        .chain(workspace_members.iter().map(PathBuf::as_path))
        .filter_map(|dir| declared_direct_range(&dir.join("package.json"), name))
        .collect()
}

fn declared_direct_range(manifest_path: &Path, name: &str) -> Option<String> {
    let manifest = super::cached_aube_manifest(manifest_path)?;
    manifest
        .dependencies
        .get(name)
        .or_else(|| manifest.dev_dependencies.get(name))
        .or_else(|| manifest.optional_dependencies.get(name))
        .cloned()
}

/// Best-effort major from a declared semver RANGE. `Some(major)` for a single
/// concrete/caret/tilde/x-range or a lower bound (`56.0.15`, `^56.0.0`, `~56`,
/// `56.x`, `v56`, `>=56`); `None` for anything we can't floor pre-resolution —
/// empty, `*`/`latest`/`x`, a compound range (whitespace / `|`), a protocol spec
/// (`:`), or an UPPER bound (`<`). `<` is rejected rather than stripped because
/// its major is the ceiling, not the floor: `<56` selects a below-floor version
/// yet reads as major 56, so flooring it would wrongly KEEP GVS — treating it as
/// ambiguous ejects instead (the safe direction). A `None` drives
/// eject-on-ambiguity in [`expo_below_gvs_floor`] and its siblings.
pub(super) fn major_floor(range: &str) -> Option<u32> {
    let r = range.trim();
    if r.is_empty() || r.contains([' ', ':', '|', '<']) {
        return None;
    }
    let core = r.trim_start_matches(['^', '~', 'v', 'V', '>', '=']);
    let digits: String = core.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_floor_parses_common_range_shapes() {
        for (range, want) in [
            ("56.0.15", Some(56)),
            ("^56.0.0", Some(56)),
            ("~56.0.0", Some(56)),
            ("56", Some(56)),
            ("56.x", Some(56)),
            ("v56.0.0", Some(56)),
            (">=56.0.0", Some(56)),
            ("52.0.0", Some(52)),
            ("^51.0.0", Some(51)),
        ] {
            assert_eq!(major_floor(range), want, "range={range}");
        }
    }

    #[test]
    fn major_floor_is_none_for_unfloorable_specs() {
        for range in [
            "",
            "*",
            "latest",
            "x",
            ">=50 <60",
            "50 - 60",
            "^55 || ^56",
            "catalog:",
            "workspace:*",
            "npm:expo@56.0.0",
            "<56",
            "<=56.0.0",
        ] {
            assert_eq!(major_floor(range), None, "range={range}");
        }
    }
}
