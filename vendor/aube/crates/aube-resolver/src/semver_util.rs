use aube_registry::Packument;

/// Outcome of [`pick_version`]. Distinguishes "nothing in the range
/// at all" from "the cutoff filtered every otherwise-satisfying
/// version" so the caller can surface a meaningful strict-mode error
/// instead of pretending the range itself was wrong.
#[derive(Debug)]
pub enum PickResult<'a> {
    Found(&'a aube_registry::VersionMetadata),
    NoMatch,
    /// Strict mode (or any caller treating the cutoff as a hard wall):
    /// at least one version satisfied the range, but all of them were
    /// filtered out by the cutoff.
    AgeGated,
}

#[cfg(test)]
impl<'a> PickResult<'a> {
    pub(crate) fn unwrap(self) -> &'a aube_registry::VersionMetadata {
        match self {
            PickResult::Found(m) => m,
            other => panic!("expected PickResult::Found, got {other:?}"),
        }
    }
}

/// Single-package version pick for `aube add`'s manifest step, honoring
/// `minimumReleaseAge` with the same dist-tag preference, exemption, and
/// strict/lenient fallback semantics [`pick_version`] applies inside full
/// resolution. Without it, `add` writes the freshly published version into
/// the manifest as a pinned spec, which the resolver's lenient fallback then
/// honors — bypassing the very gate `minimumReleaseAge` exists to provide.
///
/// `registry_name` keys the `minimumReleaseAgeExclude` match: the real
/// registry identity, not a user-facing alias. Pass `None` for
/// `minimum_release_age` to get today's ungated pick (dist-tag preference,
/// then highest satisfying).
///
/// A gated `latest` range is normalized to `*` here, at the API boundary,
/// so no caller can reintroduce the bypass: [`pick_version`]'s internal
/// dist-tag fallback turns `latest` into the tagged version's exact range,
/// whose lenient fallback would admit a fresh publish — the very thing the
/// gate exists to block. `*` keeps the dist-tag preference for a mature
/// `latest`, steers a gated one to the newest version clearing the cutoff,
/// and (unlike the tag) still resolves when `dist-tags.latest` is missing.
pub fn pick_version_for_add<'a>(
    packument: &'a Packument,
    registry_name: &str,
    range: &str,
    minimum_release_age: Option<&crate::MinimumReleaseAge>,
) -> PickResult<'a> {
    let cutoff = minimum_release_age.and_then(|m| m.cutoff());
    let range = if range == "latest" && cutoff.is_some() {
        "*"
    } else {
        range
    };
    let strict = minimum_release_age.is_some_and(|m| m.strict);
    let exclude = minimum_release_age.map(|m| &m.exclude);
    let is_age_exempt = |ver: &str, parsed: Option<&node_semver::Version>| {
        exclude.is_some_and(|ex| match parsed {
            Some(v) => ex.matches(registry_name, v),
            None => match node_semver::Version::parse(ver) {
                Ok(v) => ex.matches(registry_name, &v),
                Err(_) => ex.matches_name_only(registry_name),
            },
        })
    };
    pick_version(
        packument,
        range,
        None,
        false,
        cutoff.as_deref(),
        None,
        strict,
        is_age_exempt,
    )
}

/// Pick the best version from a packument that satisfies the given range.
///
/// `pick_lowest` flips the scan order — used by
/// `resolution-mode=time-based` for direct deps. `cutoff` filters out
/// versions whose registry publish time is later than the cutoff
/// (lexicographic compare on ISO-8601 UTC strings, which sort
/// correctly). When the packument has no `time` entry for a version
/// (e.g. abbreviated corgi payload in `Highest` mode), the cutoff is
/// ignored and the version stays eligible.
///
/// `strict` controls fallback when the cutoff filters out every
/// satisfying version: with `strict=true` we return `None` and the
/// caller errors out; with `strict=false` (the pnpm default) we make a
/// second pass that picks the *lowest* satisfying version ignoring the
/// cutoff. The lowest-satisfying fallback is pnpm's deliberate choice
/// — the oldest version in the range is least likely to be the freshly
/// pushed compromise that triggered the filter in the first place.
///
/// `is_age_exempt` lets the caller wave a specific version past the
/// cutoff — used to honor `minimumReleaseAgeExclude` (bare names, name
/// globs, and exact-version unions). It receives the candidate version
/// string plus its already-parsed form when the caller has one (every
/// hot-path call site here does, so the exemption check needn't reparse),
/// and returns `true` to treat that version as if it cleared the cutoff.
/// Pass `|_, _| false` when no exemptions apply.
///
/// `exempt_cutoff` is the time-based hard wall applied to exempt
/// versions: a version waved past the age-gate by `is_age_exempt` must
/// still clear `exempt_cutoff` (the time-based resolution cutoff). Pass
/// `None` to fully bypass the cutoff for exempt versions (no time-based
/// wall in effect).
#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn pick_version<'a>(
    packument: &'a Packument,
    range_str: &str,
    locked: Option<&str>,
    pick_lowest: bool,
    cutoff: Option<&str>,
    exempt_cutoff: Option<&str>,
    strict: bool,
    is_age_exempt: impl Fn(&str, Option<&node_semver::Version>) -> bool,
) -> PickResult<'a> {
    // Handle dist-tag references. If the requested range is a tag
    // name and the packument has that tag, use the tagged version
    // as the effective range. Special case `latest`: some registries
    // serve packuments where dist-tags.latest is absent (fresh
    // publish race, all versions deprecated, private mirror bug).
    // Old code then tried to parse "latest" as a semver range,
    // failed, returned NoMatch. Caller could not tell whether the
    // range was genuinely unsatisfiable or the tag was just missing.
    // npm and pnpm fall back to the highest non-prerelease version.
    // Do the same so `aube install foo` does not silently fail on a
    // packument that just happens to lack the tag.
    let range = match node_semver::Range::parse(normalize_range(range_str)) {
        Ok(r) => r,
        Err(_) => {
            // Reject protocol-prefixed ranges that survived workspace /
            // catalog / npm-alias preprocessing. An attacker can register
            // a dist-tag literally named `workspace:*` or `catalog:` on
            // a package they publish; without this gate the dist-tag
            // fallback below would resolve the protocol spec to whatever
            // version they pinned (dependency-confusion class). npm's
            // own dist-tag rules forbid colon in tag names but the
            // registry does not enforce that.
            if looks_like_protocol_range(range_str) {
                return PickResult::NoMatch;
            }
            let effective_range = if let Some(tagged_version) = packument.dist_tags.get(range_str) {
                tagged_version.clone()
            } else if range_str == "latest" {
                match highest_stable_version(packument) {
                    Some(v) => v,
                    None => return PickResult::NoMatch,
                }
            } else {
                return PickResult::NoMatch;
            };
            match node_semver::Range::parse(normalize_range(&effective_range)) {
                Ok(r) => r,
                Err(_) => return PickResult::NoMatch,
            }
        }
    };

    // Does `ver` clear `effective` (the cutoff that applies to it)?
    // `None` => no wall, keep the version. Missing time => keep it: we'd
    // rather risk a slightly newer transitive than fail to resolve the
    // range entirely.
    let passes_effective_cutoff = |ver: &str, effective: Option<&str>| -> bool {
        let Some(c) = effective else { return true };
        match packument.time.get(ver) {
            Some(t) => t.as_str() <= c,
            None => true,
        }
    };

    // A version's effective cutoff: exempt versions answer to the
    // time-based wall (`exempt_cutoff`) only; everyone else answers to
    // the merged `cutoff`.
    let passes_cutoff = |ver: &str, parsed: Option<&node_semver::Version>| -> bool {
        let effective = if is_age_exempt(ver, parsed) {
            exempt_cutoff
        } else {
            cutoff
        };
        passes_effective_cutoff(ver, effective)
    };

    // Prefer locked version if it satisfies and clears the cutoff.
    if let Some(locked_ver) = locked
        && let Ok(v) = node_semver::Version::parse(locked_ver)
        && v.satisfies(&range)
        && passes_cutoff(locked_ver, Some(&v))
        && let Some(meta) = packument.versions.get(locked_ver)
    {
        return PickResult::Found(meta);
    }

    // If `dist-tags.latest` satisfies the range, prefer it over the
    // strictly-highest matching version. Matches npm and pnpm: a fresh
    // `npm install foo@^1.0.0` returns the version the publisher last
    // tagged `latest`, not whatever happens to be the highest in the
    // version list (which can be a stray prerelease, hotfix on an old
    // line, or unwithdrawn experimental publish). Skipped when
    // `pick_lowest` is on (TimeBased mode wants the floor of the range,
    // not the publisher's preferred build).
    if !pick_lowest
        && let Some(latest_ver) = packument.dist_tags.get("latest")
        && let Ok(v) = node_semver::Version::parse(latest_ver)
        && v.satisfies(&range)
        && passes_cutoff(latest_ver, Some(&v))
        && let Some(meta) = packument.versions.get(latest_ver)
    {
        return PickResult::Found(meta);
    }

    // Track whether *any* version satisfied the range — if so but
    // every one was rejected by the cutoff, the failure is age-gate
    // related, not a real "no match in range".
    let mut had_satisfying_but_age_gated = false;

    let mut best: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    let mut fallback_lowest: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> =
        None;

    for (ver_str, meta) in &packument.versions {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }

        // The lenient fallback drops the minimumReleaseAge gate but never
        // the time-based hard wall, so only versions that clear
        // `exempt_cutoff` are eligible (a no-op `None` when time-based
        // mode is off).
        if passes_effective_cutoff(ver_str, exempt_cutoff)
            && fallback_lowest.as_ref().is_none_or(|(cur, _)| v < *cur)
        {
            fallback_lowest = Some((v.clone(), meta));
        }

        if passes_cutoff(ver_str, Some(&v)) {
            let replace = best
                .as_ref()
                .is_none_or(|(cur, _)| if pick_lowest { v < *cur } else { v > *cur });
            if replace {
                best = Some((v, meta));
            }
        } else {
            had_satisfying_but_age_gated = true;
        }
    }

    if let Some((_, meta)) = best {
        return PickResult::Found(meta);
    }

    // Strict mode (or no cutoff active): give up. Distinguish age-gate
    // failures so the caller can surface a meaningful error instead of
    // pretending the range itself was wrong.
    if strict || cutoff.is_none() {
        return if had_satisfying_but_age_gated {
            PickResult::AgeGated
        } else {
            PickResult::NoMatch
        };
    }

    // Lenient fallback: pnpm's `pickPackageFromMetaUsingTime` bypasses
    // the minimumReleaseAge gate and picks the *lowest* satisfying
    // version — the candidate already cleared the time-based wall above.
    if let Some((_, meta)) = fallback_lowest {
        return PickResult::Found(meta);
    }
    // Nothing left: either the range was unsatisfiable, or the
    // time-based wall excluded every satisfying version. Report the age
    // gate in the latter case so the caller surfaces a meaningful error
    // rather than a bogus "no matching version".
    if had_satisfying_but_age_gated {
        PickResult::AgeGated
    } else {
        PickResult::NoMatch
    }
}

/// True when `range_str` is a dist-tag reference (`latest`, `next`, a
/// custom tag) rather than a semver range — i.e. `pick_version` would
/// resolve it through `packument.dist_tags` (or the bare-`latest`
/// highest-stable fallback) instead of parsing it as a range.
///
/// A dist-tag is a *mutable pointer* the publisher can repoint at any
/// time, so the primer's build-time snapshot of it cannot be trusted the
/// way an immutable semver-range pick can: `classify_regime` (which keys
/// freshness on the picked version's position in settled history) is the
/// right gate for a range, but it has no way to see that a dist-tag's
/// *target* moved on the registry since the binary was built. The
/// pick-site freshness gate uses this to force a live refetch for any
/// primer-seeded dist-tag pick. Mirrors the dist-tag branch in
/// `pick_version`: a protocol-shaped spec is never a tag.
#[inline]
pub(crate) fn range_resolves_via_dist_tag(packument: &Packument, range_str: &str) -> bool {
    if node_semver::Range::parse(normalize_range(range_str)).is_ok() {
        return false;
    }
    if looks_like_protocol_range(range_str) {
        return false;
    }
    packument.dist_tags.contains_key(range_str) || range_str == "latest"
}

/// Walk the packument's versions and return the highest non
/// prerelease version string. Used as the `latest` tag fallback
/// when the registry response lacks `dist-tags.latest`. Some
/// private mirrors and mid-publish races drop the tag briefly
/// and returning NoMatch there would break `aube install foo` for
/// no real reason. npm and pnpm both fall back to highest stable.
#[inline]
/// True when `range_str` carries a URL-scheme-shaped prefix and so must
/// never reach the dist-tag fallback — workspace / catalog / file / link
/// / npm-alias / jsr-alias / git / http(s), but also any scheme aube
/// does not enumerate.
///
/// This is a *denylist by shape*, not an allowlist of known protocols.
/// The earlier allowlist (workspace/catalog/npm/.../gist) only blocked
/// the ~18 enumerated prefixes, so an attacker could pick a scheme aube
/// never listed (`evil:steal`, `patch:foo`), register a dist-tag of that
/// literal name on a package they control, and have the colon-scheme
/// spec resolve straight to it (dependency-confusion). npm forbids colons
/// in dist-tag names — and npm / pnpm / bun all reject a colon-scheme
/// spec rather than treating it as a tag — so a colon-after-a-scheme is
/// unambiguously a protocol selector, never a registry tag. We match the
/// RFC-3986 scheme grammar `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`
/// followed by `:`. A bare dist-tag (`latest`, `next`, `beta-1`) has no
/// colon and so is never blocked; this strengthens the existing guard
/// without rejecting anything that legitimately resolved before.
fn looks_like_protocol_range(range_str: &str) -> bool {
    let Some(idx) = range_str.find(':') else {
        return false;
    };
    let scheme = &range_str[..idx];
    let mut chars = scheme.chars();
    // Scheme must start with a letter, then letters/digits/`+`/`-`/`.`.
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Freshness regime of a picked version *relative to the rest of the
/// packument we can see*. Used by the primer pick-site gate to decide
/// whether an offline (primer-seeded) pick is safe to serve without a
/// freshness refetch.
///
/// The intuition: a pick is "frozen" when newer releases already exist
/// past it, so the registry can never produce a *newer* satisfying
/// answer than what we already hold — the slice we picked from is
/// immutable history and a refetch would change nothing. A pick is
/// "current/live" when nothing newer exists past it, so the registry
/// *could* have published a newer version since the primer was built
/// and a stale offline pick would silently miss it.
///
/// `HardFrozen` (a higher minor exists in the same major) is the
/// strongest signal — the user's caret range almost certainly would
/// have moved to that higher minor were it reachable, so the fact that
/// we picked below it means the range is pinned to an older line whose
/// history is settled. `SoftFrozen` (only a higher *major* exists) is
/// weaker: the next publish on the picked major line is still possible,
/// but the existence of a whole newer major strongly implies the
/// picked line is in maintenance, not active churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Regime {
    /// A higher minor (or patch within a higher minor) exists in the
    /// same major as the pick. History below it is settled.
    HardFrozen,
    /// No higher minor in the pick's major, but a higher major exists.
    /// The picked line is plausibly in maintenance.
    SoftFrozen,
    /// The pick is at (or above) the visible frontier — nothing newer
    /// exists in the packument we hold. A newer publish could exist
    /// upstream that the offline seed can't see.
    Current,
}

/// Classify the freshness regime of `picked_version` against the
/// other versions present in `packument`. Pure + total: an unparseable
/// pick, or one with no comparable peers, is treated as `Current`
/// (the conservative/posture-preserving default — never assume frozen
/// when we can't prove it).
///
/// Only *stable* (non-prerelease) versions are considered when scanning
/// for "something higher exists", to match the way ranges resolve in
/// practice — a dangling `2.0.0-rc.1` shouldn't make a `1.x` pick look
/// frozen, nor should it count as a higher major over a stable `1.x`.
#[inline]
pub(crate) fn classify_regime(packument: &Packument, picked_version: &str) -> Regime {
    let Ok(picked) = node_semver::Version::parse(picked_version) else {
        return Regime::Current;
    };
    let mut higher_minor_same_major = false;
    let mut higher_major = false;
    for ver_str in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        // Ignore prereleases — they don't gate a stable pick's regime.
        if !v.pre_release.is_empty() {
            continue;
        }
        if v <= picked {
            continue;
        }
        if v.major == picked.major {
            // Strictly newer within the same major (higher minor, or a
            // higher patch on a higher minor): hard-frozen evidence.
            higher_minor_same_major = true;
        } else if v.major > picked.major {
            higher_major = true;
        }
    }
    if higher_minor_same_major {
        Regime::HardFrozen
    } else if higher_major {
        Regime::SoftFrozen
    } else {
        Regime::Current
    }
}

#[inline]
pub(crate) fn highest_stable_version(packument: &Packument) -> Option<String> {
    let mut best: Option<(node_semver::Version, String)> = None;
    for key in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(key) else {
            continue;
        };
        // Skip prereleases so we match npm semantics. Registry
        // with only prereleases returns None and caller gets
        // NoMatch, same as before.
        if !v.pre_release.is_empty() {
            continue;
        }
        match &best {
            None => best = Some((v, key.clone())),
            Some((cur, _)) if v > *cur => best = Some((v, key.clone())),
            _ => {}
        }
    }
    best.map(|(_, k)| k)
}
/// Extract the trailing `@<version>` from an `npm:<name>@<version>`
/// or `jsr:<name>@<version>` alias spec. Returns the input unchanged
/// when the spec isn't an alias or doesn't carry a version tail.
#[inline]
pub(crate) fn strip_alias_prefix(range: &str) -> &str {
    for prefix in ["npm:", "jsr:"] {
        if let Some(rest) = range.strip_prefix(prefix) {
            return match rest.rfind('@') {
                Some(at) if at > 0 => &rest[at + 1..],
                _ => rest,
            };
        }
    }
    range
}

#[inline]
pub(crate) fn version_satisfies(version: &str, range_str: &str) -> bool {
    with_cached_version(version, |v| {
        let Some(v) = v else { return false };
        with_cached_range(normalize_range(range_str), |r| match r {
            Some(r) => v.satisfies(r),
            None => false,
        })
    })
}

/// npm / pnpm / yarn all treat an empty or whitespace-only version
/// range as equivalent to `"*"` (match any). `node_semver` rejects it
/// with `No valid ranges could be parsed`. Normalize here so the
/// resolver and every `version_satisfies` caller agree with the
/// upstream registry semantics. Real-world case: `hashring@0.0.8`
/// declares `"bisection": ""` in its dependencies.
pub(crate) fn normalize_range(range_str: &str) -> &str {
    if range_str.trim().is_empty() {
        "*"
    } else {
        range_str
    }
}

/// Thread-local `node_semver::Range` parse cache.
///
/// Resolver hot loops (sibling dedupe, lockfile-reuse scan,
/// peer-context fixed-point, catalog pick) call `version_satisfies`
/// thousands of times against a small repeating range set
/// (`"^18.2.0"`, `"*"`, `"1.x"`). Re-parsing burns CPU. Memo turns
/// 15k reparses on a 500-pkg graph into ~500 parses plus hits.
///
/// `thread_local!` beats a global mutex. Each tokio worker owns its
/// slice of ranges, lock contention would erase the parse savings.
/// Two workers parsing the same range twice is cheaper than one
/// lock round-trip.
fn with_cached_range<R>(range_str: &str, f: impl FnOnce(Option<&node_semver::Range>) -> R) -> R {
    thread_local! {
        static CACHE: std::cell::RefCell<crate::FxHashMap<String, Option<node_semver::Range>>> =
            std::cell::RefCell::default();
    }
    CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(range_str) {
            let parsed = node_semver::Range::parse(range_str).ok();
            map.insert(range_str.to_string(), parsed);
        }
        f(map.get(range_str).and_then(Option::as_ref))
    })
}

// Mirrors with_cached_range. Locked-version side hits same string
// thousands of times across peer-context + dedupe passes. Hit rate
// trends to 1.0 after first BFS layer.
fn with_cached_version<R>(version: &str, f: impl FnOnce(Option<&node_semver::Version>) -> R) -> R {
    thread_local! {
        static CACHE: std::cell::RefCell<crate::FxHashMap<String, Option<node_semver::Version>>> =
            std::cell::RefCell::default();
    }
    CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(version) {
            let parsed = node_semver::Version::parse(version).ok();
            map.insert(version.to_string(), parsed);
        }
        f(map.get(version).and_then(Option::as_ref))
    })
}
