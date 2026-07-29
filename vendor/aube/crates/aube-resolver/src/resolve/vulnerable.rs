use aube_registry::Packument;
use std::collections::BTreeMap;

pub(crate) fn is_vulnerable(
    package_name: &str,
    version: &str,
    vulnerable_ranges: &BTreeMap<String, Vec<String>>,
) -> bool {
    let Some(ranges) = vulnerable_ranges.get(package_name) else {
        return false;
    };
    let Ok(version) = node_semver::Version::parse(version) else {
        return false;
    };
    ranges
        .iter()
        .filter_map(|range| node_semver::Range::parse(range).ok())
        .any(|range| version.satisfies(&range))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prefer_non_vulnerable_pick<'a>(
    package_name: &str,
    packument: &'a Packument,
    range_str: &str,
    fallback: &'a aube_registry::VersionMetadata,
    pick_lowest: bool,
    cutoff: Option<&str>,
    exempt_cutoff: Option<&str>,
    vulnerable_ranges: &BTreeMap<String, Vec<String>>,
    is_age_exempt: impl Fn(&str, Option<&node_semver::Version>) -> bool,
) -> &'a aube_registry::VersionMetadata {
    if !is_vulnerable(package_name, &fallback.version, vulnerable_ranges) {
        return fallback;
    }
    let Ok(range) = node_semver::Range::parse(crate::semver_util::normalize_range(range_str))
    else {
        return fallback;
    };
    // Mirror `pick_version`'s cutoff: a `minimumReleaseAgeExclude` match
    // waves a version past the minimumReleaseAge gate here too (still
    // subject to `exempt_cutoff`, the time-based wall), otherwise the
    // re-pick could discard the exempt safe version and keep the
    // vulnerable one. The wall itself is the shared
    // [`crate::semver_util::version_clears_cutoff`] — this path had its own
    // copy of the comparison and so kept the missing-time fail-open after
    // `pick_version` dropped it (#581); sharing the helper is what stops the
    // two drifting again.
    let passes_cutoff = |ver: &str, parsed: Option<&node_semver::Version>| -> bool {
        let effective = if is_age_exempt(ver, parsed) {
            exempt_cutoff
        } else {
            cutoff
        };
        crate::semver_util::version_clears_cutoff(packument, ver, effective)
    };
    let mut best: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    for (ver_str, meta) in &packument.versions {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !version.satisfies(&range)
            || !passes_cutoff(ver_str, Some(&version))
            || is_vulnerable(package_name, ver_str, vulnerable_ranges)
        {
            continue;
        }
        let replace = best.as_ref().is_none_or(|(cur, _)| {
            if pick_lowest {
                version < *cur
            } else {
                version > *cur
            }
        });
        if replace {
            best = Some((version, meta));
        }
    }
    best.map(|(_, meta)| meta).unwrap_or(fallback)
}
