use aube_registry::Packument;
use std::collections::BTreeMap;

pub(super) fn is_vulnerable(
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

pub(super) fn prefer_non_vulnerable_pick<'a>(
    package_name: &str,
    packument: &'a Packument,
    range_str: &str,
    fallback: &'a aube_registry::VersionMetadata,
    pick_lowest: bool,
    cutoff: Option<&str>,
    vulnerable_ranges: &BTreeMap<String, Vec<String>>,
) -> &'a aube_registry::VersionMetadata {
    if !is_vulnerable(package_name, &fallback.version, vulnerable_ranges) {
        return fallback;
    }
    let Ok(range) = node_semver::Range::parse(crate::semver_util::normalize_range(range_str))
    else {
        return fallback;
    };
    let passes_cutoff = |ver: &str| -> bool {
        let Some(c) = cutoff else { return true };
        match packument.time.get(ver) {
            Some(t) => t.as_str() <= c,
            None => true,
        }
    };
    let mut best: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    for (ver_str, meta) in &packument.versions {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !version.satisfies(&range)
            || !passes_cutoff(ver_str)
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
