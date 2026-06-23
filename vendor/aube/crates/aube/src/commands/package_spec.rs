/// Pick the highest version in `packument` that satisfies `range_str`.
/// Returns the *original packument key* (not a round-tripped `Version`
/// display string) so string comparisons against the lockfile's
/// `current` — which also comes from a packument key — stay stable
/// for versions whose `Display` differs from their original form
/// (e.g. leading zeros in prerelease identifiers, build metadata
/// that `Version` drops). Returns `None` for unparseable ranges
/// (workspace:/file: specs, git URLs, etc.) so callers can fall
/// back to the locked version.
pub(crate) fn max_satisfying_version(
    packument: &aube_registry::Packument,
    range_str: &str,
) -> Option<String> {
    let range = node_semver::Range::parse(range_str).ok()?;
    let mut best: Option<(&str, node_semver::Version)> = None;
    for ver_str in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| v > *b) {
            best = Some((ver_str.as_str(), v));
        }
    }
    best.map(|(key, _)| key.to_string())
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
