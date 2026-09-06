#[derive(Debug, Clone)]
pub(crate) struct PolicyVersion {
    pub(crate) selected: Option<String>,
    pub(crate) blocked: Option<String>,
}

/// Pick the version a policy-aware update would actually resolve.
///
/// This delegates to the resolver's picker so dist-tag preference,
/// deprecation handling, `minimumReleaseAge` exclusions, and strict-mode
/// failures cannot drift between resolution and the human-facing commands.
/// Both `outdated` and the interactive update picker use it so neither
/// advertises a version that the resolver will reject moments later.
pub(crate) fn policy_version_info(
    packument: &aube_registry::Packument,
    registry_name: &str,
    range_str: &str,
    minimum_release_age: Option<&aube_resolver::MinimumReleaseAge>,
    current_version: Option<&str>,
) -> PolicyVersion {
    // Human-facing update reports preserve an absent `latest` tag as
    // unknown. The resolver itself may fall back to the highest stable
    // release so installs remain possible, but presenting that fallback as
    // the publisher's `latest` tag would make the report misleading.
    if range_str == "latest" && !packument.dist_tags.contains_key("latest") {
        return PolicyVersion {
            selected: None,
            blocked: None,
        };
    }
    let policy_selected = match aube_resolver::pick_version_for_add(
        packument,
        registry_name,
        range_str,
        minimum_release_age,
    ) {
        aube_resolver::PickResult::Found(meta) => Some(meta.version.clone()),
        aube_resolver::PickResult::NoMatch | aube_resolver::PickResult::AgeGated(_) => None,
    };
    // An already-installed young release is allowed to remain in place.
    // Never advertise a downgrade or call that same release "hidden".
    let selected = match (policy_selected, current_version) {
        (Some(selected), Some(current)) if is_newer(current, &selected) => {
            Some(current.to_string())
        }
        (None, Some(current)) => Some(current.to_string()),
        (selected, _) => selected,
    };
    let blocked = minimum_release_age.and_then(|_| {
        let ungated =
            match aube_resolver::pick_version_for_add(packument, registry_name, range_str, None) {
                aube_resolver::PickResult::Found(meta) => Some(meta.version.clone()),
                aube_resolver::PickResult::NoMatch | aube_resolver::PickResult::AgeGated(_) => None,
            }?;
        selected
            .as_deref()
            .is_some_and(|baseline| is_newer(&ungated, baseline))
            .then_some(ungated)
    });
    PolicyVersion { selected, blocked }
}

fn is_newer(candidate: &str, selected: &str) -> bool {
    match (
        node_semver::Version::parse(candidate),
        node_semver::Version::parse(selected),
    ) {
        (Ok(candidate), Ok(selected)) => candidate > selected,
        _ => candidate != selected,
    }
}

pub(crate) fn record_age_gated_update(
    blocked: &mut std::collections::BTreeMap<String, String>,
    name: String,
    version: String,
) {
    let replace = blocked
        .get(&name)
        .is_none_or(|existing| is_newer(&version, existing));
    if replace {
        blocked.insert(name, version);
    }
}

pub(crate) fn warn_age_gated_updates(blocked: &std::collections::BTreeMap<String, String>) {
    if blocked.is_empty() {
        return;
    }
    let packages = blocked
        .iter()
        .map(|(name, version)| format!("{name}@{version}"))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_MINIMUM_RELEASE_AGE_BLOCKED_UPDATE,
        count = blocked.len(),
        packages,
        "updates hidden by minimumReleaseAge: {packages}"
    );
}

/// Pick the highest version in the packument satisfying `range_str`, preferring
/// `dist-tags.latest` when it satisfies the range and a non-deprecated release
/// over a deprecated one.
///
/// Returns the *original packument key* (not a round-tripped `Version`
/// display string) so string comparisons against the lockfile's
/// `current` — which also comes from a packument key — stay stable
/// for versions whose `Display` differs from their original form
/// (e.g. leading zeros in prerelease identifiers, build metadata
/// that `Version` drops). Returns `None` for unparseable ranges
/// (workspace:/file: specs, git URLs, etc.) so callers can fall
/// back to the locked version.
pub(crate) fn wanted_version(
    packument: &aube_registry::Packument,
    range_str: &str,
) -> Option<String> {
    let range = node_semver::Range::parse(range_str).ok()?;
    if let Some(latest) = packument.dist_tags.get("latest")
        && packument.versions.contains_key(latest)
        && let Ok(v) = node_semver::Version::parse(latest)
        && v.satisfies(&range)
    {
        return Some(latest.clone());
    }
    let mut best: Option<(&str, node_semver::Version, bool)> = None;
    for (ver_str, meta) in &packument.versions {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }
        let live = meta.deprecated.is_none();
        let outranks = match &best {
            None => true,
            Some((_, _, best_live)) if live != *best_live => live,
            Some((_, best_v, _)) => v > *best_v,
        };
        if outranks {
            best = Some((ver_str.as_str(), v, live));
        }
    }
    best.map(|(key, _, _)| key.to_string())
}

/// Resolve a version spec against a full packument. Returns the concrete
/// version string to look up in the `versions` object.
///
/// Resolution order, matching npm/pnpm:
/// 1. No spec → `dist-tags.latest`
/// 2. Spec is a dist-tag → `dist-tags[spec]`
/// 3. Spec is an exact version in `versions` → that version
/// 4. Spec is a semver range → highest matching version in `versions`
///
/// Shared by `aube view` and `aube store add` so fixes land in one place.
pub(crate) fn resolve_version(packument: &serde_json::Value, spec: Option<&str>) -> Option<String> {
    let dist_tags = packument.get("dist-tags").and_then(|v| v.as_object());
    let versions = packument.get("versions").and_then(|v| v.as_object())?;

    let spec = match spec {
        None | Some("") => {
            return dist_tags?
                .get("latest")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        Some(s) => s,
    };

    if let Some(tag) = dist_tags.and_then(|t| t.get(spec)).and_then(|v| v.as_str()) {
        return Some(tag.to_string());
    }

    if versions.contains_key(spec) {
        return Some(spec.to_string());
    }

    let range: node_semver::Range = spec.parse().ok()?;
    versions
        .keys()
        .filter_map(|v| {
            v.parse::<node_semver::Version>()
                .ok()
                .filter(|parsed| parsed.satisfies(&range))
                .map(|parsed| (v.clone(), parsed))
        })
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(raw, _)| raw)
}

/// Split `name[@version]` into the package name and optional version spec.
/// Handles scoped packages (`@scope/name[@version]`) correctly — the first
/// `@` in a scoped input is the scope sigil, not a version separator.
///
/// Returns borrowed slices of the input. Callers that need owned `String`s
/// or a default like `"latest"` can adapt the result at their call site.
pub(crate) fn split_name_spec(input: &str) -> (&str, Option<&str>) {
    aube_util::pkg::split_name_spec(input)
}

/// Percent-encode a package name for npm registry path segments.
/// `@scope/name` becomes `@scope%2Fname`; the leading `@` stays literal
/// and only the scope/name slash is encoded. Plain names pass through.
///
/// Shared between `publish` and `unpublish` (both target
/// `{registry}/{name}/...` endpoints) so the two write commands can't
/// drift on URL shape — the registry routes auth on these paths, so
/// even a subtle encoding change would break one command silently
/// while leaving the other working.
pub(crate) fn encode_package_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix('@')
        && let Some((scope, pkg)) = rest.split_once('/')
    {
        return format!("@{scope}%2F{pkg}");
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted_version(packument: &aube_registry::Packument, range: &str) -> Option<String> {
        policy_version_info(packument, &packument.name, range, None, None).selected
    }

    fn packument(json: serde_json::Value) -> aube_registry::Packument {
        serde_json::from_value(json).expect("test packument parses")
    }

    /// Shape of the real `codemirror` packument: `6.65.7` is an
    /// accidentally mis-tagged republish of `5.65.7`, deprecated by the
    /// maintainer, and sorts above every genuine 6.x release.
    fn codemirror() -> aube_registry::Packument {
        packument(serde_json::json!({
            "name": "codemirror",
            "dist-tags": { "latest": "6.0.2", "version5": "5.65.21" },
            "versions": {
                "6.0.1": { "name": "codemirror", "version": "6.0.1" },
                "6.0.2": { "name": "codemirror", "version": "6.0.2" },
                "6.65.7": {
                    "name": "codemirror",
                    "version": "6.65.7",
                    "deprecated": "This is an accidentally mis-tagged instance of 5.65.7"
                }
            }
        }))
    }

    /// Shape of the real `graceful-fs` packument: `4.2.7` is deprecated
    /// in favor of `4.2.8+`, and `dist-tags.latest` (4.2.11) sits above
    /// any range pinned to the 4.2.0–4.2.7 window.
    fn graceful_fs() -> aube_registry::Packument {
        packument(serde_json::json!({
            "name": "graceful-fs",
            "dist-tags": { "latest": "4.2.11" },
            "versions": {
                "4.2.6": { "name": "graceful-fs", "version": "4.2.6" },
                "4.2.7": {
                    "name": "graceful-fs",
                    "version": "4.2.7",
                    "deprecated": "Please upgrade to v4.2.8 or higher"
                },
                "4.2.11": { "name": "graceful-fs", "version": "4.2.11" }
            }
        }))
    }

    #[test]
    fn wanted_skips_deprecated_when_the_range_admits_another() {
        // Regression: `aube outdated` reported `codemirror` as
        // 6.0.2 → 6.65.7 (wanted) even though the registry marks
        // 6.65.7 deprecated and `dist-tags.latest` is 6.0.2.
        assert_eq!(
            wanted_version(&codemirror(), "^6.0.2").as_deref(),
            Some("6.0.2")
        );
        // Same rule with the dist-tag out of the picture: 4.2.11 is
        // outside the range, so the scan runs and must step over the
        // deprecated 4.2.7 down to 4.2.6.
        assert_eq!(
            wanted_version(&graceful_fs(), ">=4.2.0 <4.2.8").as_deref(),
            Some("4.2.6")
        );
    }

    #[test]
    fn wanted_keeps_deprecated_when_nothing_else_matches() {
        // pnpm parity: the deprecation preference is a tiebreak, not a
        // filter — a range that only reaches deprecated versions still
        // resolves rather than reporting nothing.
        assert_eq!(
            wanted_version(&codemirror(), "^6.65.0").as_deref(),
            Some("6.65.7")
        );
        assert_eq!(
            wanted_version(&graceful_fs(), "4.2.7").as_deref(),
            Some("4.2.7")
        );
    }

    #[test]
    fn wanted_keeps_a_deprecated_latest_dist_tag() {
        // `is-odd@3.0.1` is both deprecated *and* the `latest` tag. The
        // resolver installs it (pnpm returns the tag before it ever
        // looks at deprecation), so the report has to agree — otherwise
        // `outdated` proposes 3.0.1 → 3.0.0, a downgrade to a version
        // no install would ever pick.
        let is_odd = packument(serde_json::json!({
            "name": "is-odd",
            "dist-tags": { "latest": "3.0.1" },
            "versions": {
                "3.0.0": { "name": "is-odd", "version": "3.0.0" },
                "3.0.1": {
                    "name": "is-odd",
                    "version": "3.0.1",
                    "deprecated": "please use is-even"
                }
            }
        }));
        assert_eq!(wanted_version(&is_odd, "^3.0.0").as_deref(), Some("3.0.1"));
    }

    #[test]
    fn wanted_prefers_the_latest_dist_tag_over_a_higher_in_range_version() {
        // pnpm parity: the publisher's tag anchors the canonical
        // install, so a higher in-range version that isn't tagged
        // `latest` must not be reported as wanted.
        assert_eq!(
            wanted_version(&graceful_fs(), "*").as_deref(),
            Some("4.2.11")
        );
        assert_eq!(wanted_version(&codemirror(), "*").as_deref(), Some("6.0.2"));
    }

    #[test]
    fn policy_version_skips_fresh_latest_release() {
        let package = packument(serde_json::json!({
            "name": "demo",
            "dist-tags": { "latest": "2.0.0" },
            "versions": {
                "1.0.0": { "name": "demo", "version": "1.0.0" },
                "2.0.0": { "name": "demo", "version": "2.0.0" }
            },
            "time": {
                "1.0.0": "2000-01-01T00:00:00.000Z",
                "2.0.0": "2999-01-01T00:00:00.000Z"
            }
        }));
        let minimum_release_age = aube_resolver::MinimumReleaseAge {
            minutes: 1440,
            exclude: aube_resolver::PackageVersionPolicy::default(),
            strict: false,
        };

        assert_eq!(
            policy_version_info(&package, "demo", "latest", Some(&minimum_release_age), None,)
                .selected
                .as_deref(),
            Some("1.0.0")
        );
        assert_eq!(
            policy_version_info(&package, "demo", "*", Some(&minimum_release_age), None)
                .selected
                .as_deref(),
            Some("1.0.0")
        );
        let info =
            policy_version_info(&package, "demo", "latest", Some(&minimum_release_age), None);
        assert_eq!(info.blocked.as_deref(), Some("2.0.0"));

        let installed = policy_version_info(
            &package,
            "demo",
            "latest",
            Some(&minimum_release_age),
            Some("2.0.0"),
        );
        assert_eq!(installed.selected.as_deref(), Some("2.0.0"));
        assert_eq!(installed.blocked, None);

        let alias = policy_version_info(
            &package,
            "demo",
            "npm:demo@latest",
            Some(&minimum_release_age),
            None,
        );
        assert_eq!(alias.selected.as_deref(), Some("1.0.0"));
        assert_eq!(alias.blocked.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn blocked_update_map_keeps_the_newest_version() {
        let mut blocked = std::collections::BTreeMap::new();
        record_age_gated_update(&mut blocked, "demo".to_string(), "2.0.0".to_string());
        record_age_gated_update(&mut blocked, "demo".to_string(), "3.0.0".to_string());
        record_age_gated_update(&mut blocked, "demo".to_string(), "1.0.0".to_string());
        assert_eq!(blocked.get("demo").map(String::as_str), Some("3.0.0"));
    }

    #[test]
    fn scoped_name_encodes_slash() {
        assert_eq!(encode_package_name("@scope/pkg"), "@scope%2Fpkg");
    }

    #[test]
    fn plain_name_passthrough() {
        assert_eq!(encode_package_name("lodash"), "lodash");
    }

    #[test]
    fn malformed_scoped_name_passthrough() {
        // `@scope` with no slash isn't a valid package name, but we
        // shouldn't panic — return it verbatim so the registry can
        // surface the error.
        assert_eq!(encode_package_name("@scope"), "@scope");
    }

    #[test]
    fn split_plain_name() {
        assert_eq!(split_name_spec("lodash"), ("lodash", None));
    }

    #[test]
    fn split_name_with_version() {
        assert_eq!(
            split_name_spec("lodash@4.17.21"),
            ("lodash", Some("4.17.21"))
        );
    }

    #[test]
    fn split_name_with_range() {
        assert_eq!(split_name_spec("lodash@^4"), ("lodash", Some("^4")));
    }

    #[test]
    fn split_name_with_tag() {
        assert_eq!(split_name_spec("react@next"), ("react", Some("next")));
    }

    #[test]
    fn split_scoped_no_version() {
        assert_eq!(split_name_spec("@babel/core"), ("@babel/core", None));
    }

    #[test]
    fn split_scoped_with_version() {
        assert_eq!(
            split_name_spec("@babel/core@7.0.0"),
            ("@babel/core", Some("7.0.0"))
        );
    }
}
