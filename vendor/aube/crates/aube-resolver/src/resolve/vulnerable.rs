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
    // vulnerable one.
    let passes_cutoff = |ver: &str, parsed: Option<&node_semver::Version>| -> bool {
        let effective = if is_age_exempt(ver, parsed) {
            exempt_cutoff
        } else {
            cutoff
        };
        crate::semver_util::version_clears_cutoff(packument, ver, effective, true)
    };

    // Two tiers, because this function is a PREFERENCE, not a gate: its only
    // caller already holds a version it knows to be vulnerable, and whatever
    // this returns is what gets installed. So the age wall ranks candidates
    // here; it never vetoes one down to the vulnerable fallback.
    //
    // Tier 1 is a non-vulnerable version that also clears the wall. Tier 2 is
    // a non-vulnerable version whose publish age is merely UNDETERMINABLE.
    // Tier 2 still beats returning `fallback`: a confirmed advisory hit is a
    // concrete harm, an unprovable publish date is a hypothetical one, and
    // trading the former for the latter is not a trade worth making. This is
    // the deliberate asymmetry with `pick_version`, which has no such
    // second-worst option to fall to and so blocks outright (#581).
    //
    // A version with a KNOWN publish time that is too new belongs to NEITHER
    // tier: that is the freshly-published-compromise case the gate exists to
    // stop, and it stays excluded even against a vulnerable fallback.
    let mut best: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    let mut best_undated: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    for (ver_str, meta) in &packument.versions {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !version.satisfies(&range) || is_vulnerable(package_name, ver_str, vulnerable_ranges) {
            continue;
        }
        let tier = if passes_cutoff(ver_str, Some(&version)) {
            &mut best
        } else if packument.time.contains_key(ver_str) {
            // Dated and too new — excluded outright.
            continue;
        } else {
            &mut best_undated
        };
        let replace = tier.as_ref().is_none_or(|(cur, _)| {
            if pick_lowest {
                version < *cur
            } else {
                version > *cur
            }
        });
        if replace {
            *tier = Some((version, meta));
        }
    }
    best.or(best_undated)
        .map(|(_, meta)| meta)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::make_packument;

    const CUTOFF: &str = "2020-01-01T00:00:00.000Z";

    fn ranges(name: &str, range: &str) -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([(name.to_string(), vec![range.to_string()])])
    }

    /// The re-pick ranks by publish age but never vetoes down to the
    /// vulnerable fallback: an undated non-vulnerable version is a better
    /// outcome than a confirmed advisory hit. A DATED-but-too-new version is
    /// still excluded outright — that is the fresh-compromise case.
    #[test]
    fn undated_safe_version_beats_a_vulnerable_fallback_but_a_fresh_one_does_not() {
        let vuln = ranges("foo", "<1.1.0");

        // 1.1.0 has no time entry; 1.0.0 is vulnerable. Prefer the undated
        // safe version rather than keeping the known-vulnerable one.
        let mut undated = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        undated
            .time
            .insert("1.0.0".to_string(), "2019-01-01T00:00:00.000Z".to_string());
        let fallback = undated.versions.get("1.0.0").unwrap();
        let picked = prefer_non_vulnerable_pick(
            "foo",
            &undated,
            "^1.0.0",
            fallback,
            false,
            Some(CUTOFF),
            None,
            &vuln,
            |_, _| false,
        );
        assert_eq!(picked.version, "1.1.0");

        // Same shape, but 1.1.0 is dated and too new: its age is KNOWN to
        // violate the floor, so it stays excluded and the fallback stands.
        let mut fresh = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        fresh
            .time
            .insert("1.0.0".to_string(), "2019-01-01T00:00:00.000Z".to_string());
        fresh
            .time
            .insert("1.1.0".to_string(), "2026-01-01T00:00:00.000Z".to_string());
        let fallback = fresh.versions.get("1.0.0").unwrap();
        let picked = prefer_non_vulnerable_pick(
            "foo",
            &fresh,
            "^1.0.0",
            fallback,
            false,
            Some(CUTOFF),
            None,
            &vuln,
            |_, _| false,
        );
        assert_eq!(picked.version, "1.0.0");
    }
}
