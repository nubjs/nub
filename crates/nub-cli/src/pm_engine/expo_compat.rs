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
//! Defaults are computed before resolution, so only the DECLARED range in the
//! root manifest is available (not a resolved version). A range whose major
//! can't be read pre-resolution (`*`, `latest`, a compound `>=50 <60`, a
//! `catalog:`/`workspace:` protocol spec) is treated as below-floor and EJECTS —
//! the safe direction, matching `react-native`: the worst case is a redundant
//! project-local install, never a broken one. The residual case the version
//! gate can't see is a 56+ project that disables On-demand FS via
//! `experiments.onDemandFilesystem: false`, which lives in `app.json`/
//! `app.config.*` rather than `package.json`; that is an accepted, documented
//! edge case.

use std::path::Path;

/// The `expo` major at and above which the On-demand Filesystem makes a project
/// GVS-compatible (Expo SDK 56).
const EXPO_GVS_FLOOR: u32 = 56;

/// Whether the project at `root` declares an `expo` dependency whose SDK is
/// below the GVS floor — i.e. GVS must be ejected for it. `false` when `expo`
/// is not a direct dependency (not an Expo project) or its declared major is
/// `>= EXPO_GVS_FLOOR`; `true` when it is below the floor OR the range can't be
/// floor-parsed (eject-on-ambiguity). Matches the aube trigger's dependency
/// scope (dependencies / devDependencies / optionalDependencies; peer excluded).
pub(crate) fn expo_below_gvs_floor(root: &Path) -> bool {
    let Some(range) = declared_expo_range(root) else {
        return false;
    };
    match expo_major_floor(&range) {
        Some(major) => major < EXPO_GVS_FLOOR,
        None => true,
    }
}

/// The declared `expo` range from the root manifest's direct-dependency fields,
/// if any. Uses the shared mtime-cached parse so the extra read is free.
fn declared_expo_range(root: &Path) -> Option<String> {
    let manifest = super::cached_aube_manifest(&root.join("package.json"))?;
    manifest
        .dependencies
        .get("expo")
        .or_else(|| manifest.dev_dependencies.get("expo"))
        .or_else(|| manifest.optional_dependencies.get("expo"))
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
/// eject-on-ambiguity in [`expo_below_gvs_floor`].
fn expo_major_floor(range: &str) -> Option<u32> {
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
            assert_eq!(expo_major_floor(range), want, "range={range}");
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
            assert_eq!(expo_major_floor(range), None, "range={range}");
        }
    }
}
