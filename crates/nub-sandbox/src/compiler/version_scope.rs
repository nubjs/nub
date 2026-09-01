//! Whether a version-scoped catalog entry applies to the package version being confined.
//!
//! One predicate, shared by every table that carries a scope — v1's [`super::curated`] and
//! [`super::package_network`], and the v2 catalog's version bands
//! ([`crate::catalog_v2::Entry::grant_for`]) — because a grant that applied on one axis and not
//! the other for the same package would be the silent-wrong-answer shape this whole subsystem
//! is built against.
//!
//! # Why version scoping is worth having at all
//!
//! The ecosystem moves TOWARD prebuilt `optionalDependencies` and away from building at
//! install time, so the versions that need a capability are the OLD ones and a cutoff gets
//! more accurate over time rather than less. `esbuild` is the measured case: its break
//! boundary is exactly `0.13.0`, the version `optionalDependencies` landed, and by weekly
//! downloads the versions below it are 0.125% of its installs. A name-keyed grant hands the
//! other 99.82% a capability they provably do not use.
//!
//! # The two conservative readings, both deliberate
//!
//! An UNSCOPED entry applies to every version — that is the default the whole catalog was
//! written under, and it is what keeps this field additive.
//!
//! A SCOPED entry needs a version it can actually judge, so an absent or unparseable one does
//! NOT match. That direction withholds rather than widens, which is the same reading the
//! tables already take for an absent package name.

/// Whether an entry scoped to `range` applies to `version`.
///
/// A SCOPED ENTRY DOES NOT REACH PRERELEASES, and the reason is stronger than it looks.
/// `semver` admits a prerelease only when some comparator carries a prerelease at the SAME
/// major.minor.patch, so a cutoff cannot be widened into one: measured, `<0.13.0`,
/// `<0.13.0-0` and `>=0.0.0-0, <0.13.0` all refuse `0.12.0-rc.1`, and only a bound naming
/// that version's own release (`>=0.12.0-rc, <0.13.0`) admits it. Where that lands depends on
/// the caller: a v1 scoped entry falls back to no grant and fails as if unlisted, while a v2
/// band falls through to the entry's `default` — the grant measured at `latest`, which is a
/// strictly better answer for a prerelease than nothing.
///
/// Accepted rather than worked around — but NOT for the reason this comment used to give. It
/// claimed no catalogued package ships an install-scripted prerelease. That is false, and the
/// catalog’s own bytes refute it: 21 of the 294 entries name a MEASURED prerelease in their band
/// notes (`@netlify/esbuild@0.14.39-1`, `@opencode-ai/cli@0.0.0-next-16573`,
/// `@heroui/shared-utils@0.0.0-canary-20251004132934`, …), and for two of them a prerelease is the
/// CURRENT dist-tag. So this rule reaches real packages routinely rather than never.
///
/// What carries it is the v2 fallthrough named above: a prerelease lands on the entry’s `default`.
/// That makes `default` the entire safety margin for every prerelease — which is why the real
/// hazard is a FABRICATED empty `default`, not this rule. The generator emits `{}` when `latest`
/// was never measured while still writing `latest measured X`, and a present-but-empty entry
/// denies MORE than having no entry at all, so a prerelease dist-tag lands on a hard deny. See
/// `tests/default_band_grants.rs`.
///
/// The rule itself stands, on the reasons that survive: for a v1 scoped entry the direction is
/// withhold-not-widen, and a hand-rolled precedence dialect would be a second range language for
/// one corner. When it bites, widen that entry — drop the scope, or name the prerelease bound —
/// rather than changing this rule.
pub(crate) fn applies(range: Option<&str>, version: Option<&str>) -> bool {
    let Some(range) = range else {
        return true;
    };
    let (Some(version), Ok(req)) = (version, semver::VersionReq::parse(range)) else {
        return false;
    };
    semver::Version::parse(version).is_ok_and(|v| req.matches(&v))
}

#[cfg(test)]
mod tests {
    use super::applies;

    /// The whole predicate, in the four states a caller can reach it in. The unscoped arm is
    /// the load-bearing one — all but FOUR shipped entries are in it, so a regression that made
    /// a missing range mean "no versions" would silently disarm almost the entire table.
    ///
    /// ⛔ The four, counted from the shipped catalog rather than remembered: `@sentry/cli
    /// <2.23.0`, `bcrypt <6.0.0`, `esbuild <0.13.0`, `sharp <0.33.0`. This comment said "every
    /// entry but `esbuild`" until the count was checked, and the error is not cosmetic — a
    /// grant is the (name, range) PAIR, so a name compiled at a version its range EXCLUDES is
    /// denied by design. A sweep that pins every name to one arbitrary version therefore scores
    /// `esbuild`'s and `sharp`'s deliberate refusals as broken grants. Sweep each name at a
    /// version its OWN range admits.
    #[test]
    fn an_unscoped_entry_covers_everything_and_a_scoped_one_needs_a_judgeable_version() {
        assert!(applies(None, Some("0.25.12")));
        assert!(
            applies(None, None),
            "an unscoped entry cannot depend on a version it never asked for"
        );

        assert!(applies(Some("<0.13.0"), Some("0.11.23")));
        assert!(applies(Some("<0.13.0"), Some("0.12.29")));
        assert!(!applies(Some("<0.13.0"), Some("0.13.0")));
        assert!(!applies(Some("<0.13.0"), Some("0.25.12")));

        // Withhold rather than widen when the version cannot be judged: aube hands over no
        // identity for a fetched checkout, and a `file:`/workspace pin need not be semver.
        assert!(!applies(Some("<0.13.0"), None));
        assert!(!applies(Some("<0.13.0"), Some("workspace:*")));
        assert!(!applies(Some("<0.13.0"), Some("")));

        // `semver`'s prerelease rule, pinned so a crate upgrade that changed it is visible
        // here rather than in a grant that silently started or stopped applying. The three
        // refusals are the ones a catalog author would reach for to widen a cutoff, and none
        // of them work — only a bound at the prerelease's own release does.
        assert!(!applies(Some("<0.13.0"), Some("0.12.0-rc.1")));
        assert!(!applies(Some("<0.13.0-0"), Some("0.12.0-rc.1")));
        assert!(!applies(Some(">=0.0.0-0, <0.13.0"), Some("0.12.0-rc.1")));
        assert!(applies(Some(">=0.12.0-rc, <0.13.0"), Some("0.12.0-rc.1")));
    }
}
