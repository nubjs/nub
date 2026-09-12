//! The `defaultTrust` build-policy floor.
//!
//! Off by default (`defaultTrust = false`). When enabled, it slots
//! one layer into the dependency build-script precedence chain:
//!
//! 1. explicit `allowBuilds` entry — `true`/`false` always wins, in
//!    both directions (`false` carves a package out of every layer
//!    below, including `dangerouslyAllowAllBuilds`),
//! 2. `dangerouslyAllowAllBuilds` — the allow-all posture,
//! 3. **this floor** — a curated default-trust list, per-package
//!    gated (see below),
//! 4. default deny + the unreviewed-builds warning.
//!
//! Layers 1–2 are [`aube_scripts::BuildPolicy`]; the floor is
//! consulted only on [`aube_scripts::AllowDecision::Unspecified`], so
//! its existence never overrides an explicit decision, and the mere
//! existence of an `allowBuilds` map does not disable it.
//!
//! A listed package ([`aube_scripts::is_default_trusted`], Bun's
//! vendored list plus the additions pnpm's trusted-dependencies list
//! carries) is trusted only when every gate holds:
//!
//! - **registry provenance** — the package resolved from a registry.
//!   git / file / link / tarball sources never qualify, so a malicious
//!   manifest can't borrow a listed name's trust by pointing it at an
//!   arbitrary source (the bun CVE-2026-24910 class).
//! - **advisory vetting** — either an OSV `MAL-*` advisory gate ran
//!   against this install's graph (`run_post_resolve_osv_routing`
//!   returned true), *or* the graph was inherited from an unchanged
//!   lockfile that was advisory-checked when it was written
//!   (`lockfile_vetted`). Advisory hits abort the install before
//!   scripts run, so surviving packages passed. The lockfile-inherited
//!   branch is what makes a frozen install (`aube ci`,
//!   `--frozen-lockfile`, a CI/teammate clone) run trusted build
//!   scripts without a per-install OSV round-trip — they ran for
//!   whoever locked the file, and the lockfile carries that vetting.
//!   On a fresh resolve with every advisory backend off and no prior
//!   lockfile to inherit from, the floor turns off with the gate.
//! - **cooling window** — the resolved version's recorded publish time
//!   (the lockfile-graph `time:` data the resolver records in-memory
//!   whenever `minimumReleaseAge` is active) is older than the
//!   `minimumReleaseAge` window. Unknown publish time fails closed for
//!   a **new** pick — a missing time there means the package never
//!   cleared resolution-time vetting. For a pick **inherited** from the
//!   prior lockfile at the same version an unknown time instead
//!   *waives* the window: since #892 (commit `2b61eaa`) the lockfile no
//!   longer persists `time:` data under non-time-based resolution, so
//!   an inherited pick legitimately has no time, and the cooling window
//!   — a resolution-time defense against pulling a brand-new version —
//!   has nothing to defend against for a pick that pulls nothing new. A
//!   known-but-too-young time still denies; only the *absence* is
//!   waived, and only for an inherited pick. This is decided
//!   per-package ([`InheritedPicks`]), so one new pick does not revoke
//!   the waiver for the rest of the graph. The other
//!   gates (registry provenance, advisory vetting, the allowlist) still
//!   apply. `minimumReleaseAge = 0` disables the floor entirely — it
//!   consults the cooling defense, it never substitutes for it.
//!
//! `aube rebuild` does not consult the floor: no advisory gate runs
//! there, and `aube rebuild <name>` already bypasses the policy for
//! explicitly named packages.

use aube_scripts::AllowDecision;
use std::collections::{BTreeMap, HashSet};

/// Which of this install's resolved picks were already in the prior
/// lockfile at the same version.
///
/// The cooling window's unknown-publish-time arm waives the window for
/// an inherited pick, because such a pick pulls nothing new and the
/// window is a resolution-time defense against pulling something new
/// (module docs above). **That question is per-package.** An `aube add`
/// re-resolve produces one graph holding both kinds at once — the added
/// package is new, every unrelated package is reproduced unchanged —
/// and the whole-graph `lockfile_vetted` boolean cannot tell them
/// apart. Answering it with that boolean made adding any dependency
/// un-trust every *other* package's build scripts, so `aube install`
/// and `aube add` disagreed about the same package in the same tree
/// under the same config (nubjs/nub#764).
#[derive(Debug, Clone)]
pub(crate) enum InheritedPicks {
    /// The graph came straight from the lockfile, so every pick is
    /// inherited: frozen install, `aube ci`, a teammate clone.
    All,
    /// The resolver ran. Canonical `name@version` keys that were in the
    /// prior lockfile; anything else in the graph is a new pick. Empty
    /// when there was no prior lockfile to inherit from.
    FromPriorLockfile(HashSet<String>),
}

impl InheritedPicks {
    /// Canonical keys of every registry-resolved package in the prior
    /// lockfile. Deliberately mirrors `lockfile_has_new_picks` — same
    /// `(registry_name, version)` identity, same `local_source.is_none()`
    /// filter — so the two never disagree about what counts as new.
    pub(crate) fn from_prior(prior: Option<&aube_lockfile::LockfileGraph>) -> Self {
        Self::FromPriorLockfile(
            prior
                .map(|g| {
                    g.packages
                        .values()
                        .filter(|p| p.local_source.is_none())
                        .map(|p| format!("{}@{}", p.registry_name(), p.version))
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    /// `canonical` is the `name@version` spelling the caller already
    /// built for the publish-time lookup.
    fn contains(&self, canonical: &str) -> bool {
        match self {
            Self::All => true,
            Self::FromPriorLockfile(keys) => keys.contains(canonical),
        }
    }
}

/// Resolved floor state for one install. Construct via
/// [`DefaultTrustFloor::from_settings`] after the post-resolve OSV
/// routing has run (its return value is the `osv_gate_active` input),
/// or [`DefaultTrustFloor::disabled`] on paths that must never floor
/// (rebuild).
#[derive(Debug, Clone)]
pub(crate) struct DefaultTrustFloor {
    enabled: bool,
    osv_gate_active: bool,
    /// True when this install's graph came from an unchanged lockfile
    /// (frozen install, `aube ci`, `--frozen-lockfile`, a CI/teammate
    /// clone). The graph was advisory-checked when the lockfile was
    /// written, so the floor inherits that resolution-time vetting and
    /// does not require a per-install OSV run — see
    /// `wiki/commands/pm/supply-chain-posture.md` Decision 2. The
    /// cooling window (checked against the lockfile's recorded publish
    /// times) and registry-provenance gates still apply, so the
    /// inherited trust is bounded to what the lockfile actually
    /// evidences.
    lockfile_vetted: bool,
    /// Per-package counterpart to `lockfile_vetted`, consulted by the
    /// cooling window's unknown-publish-time arm and nothing else. See
    /// [`InheritedPicks`] for why that arm cannot use the boolean above:
    /// the advisory gate `lockfile_vetted` feeds really is whole-graph,
    /// the cooling window really is not, and one field was answering
    /// both.
    inherited: InheritedPicks,
    /// ISO-8601 UTC cutoff derived from `minimumReleaseAge`: publish
    /// times lexicographically `<=` this string satisfy the window.
    /// `None` when the window is disabled — which disables the floor.
    age_cutoff: Option<String>,
}

impl DefaultTrustFloor {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            osv_gate_active: false,
            lockfile_vetted: false,
            inherited: InheritedPicks::FromPriorLockfile(HashSet::new()),
            age_cutoff: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_enabled(age_cutoff: &str) -> Self {
        Self {
            enabled: true,
            osv_gate_active: true,
            lockfile_vetted: false,
            inherited: InheritedPicks::FromPriorLockfile(HashSet::new()),
            age_cutoff: Some(age_cutoff.to_string()),
        }
    }

    /// Resolve the floor from settings. `mra_cli_minutes` is the same
    /// CLI override the resolver's `minimumReleaseAge` receives so the
    /// floor and the resolver agree on the window.
    pub(crate) fn from_settings(
        ctx: &aube_settings::ResolveCtx<'_>,
        mra_cli_minutes: Option<u64>,
        osv_gate_active: bool,
        lockfile_vetted: bool,
        inherited: InheritedPicks,
    ) -> Self {
        let enabled = aube_settings::resolved::default_trust(ctx);
        if !enabled {
            return Self::disabled();
        }
        let minutes =
            mra_cli_minutes.unwrap_or_else(|| aube_settings::resolved::minimum_release_age(ctx));
        let age_cutoff = aube_resolver::MinimumReleaseAge {
            minutes,
            ..Default::default()
        }
        .cutoff();
        Self {
            enabled,
            osv_gate_active,
            lockfile_vetted,
            inherited,
            age_cutoff,
        }
    }

    /// Cheap pre-check: could this floor trust *anything*? Lets the
    /// dep-script phase keep its "no allow rules → skip entirely"
    /// fast path when the floor can't fire anyway.
    pub(crate) fn may_allow_any(&self) -> bool {
        self.enabled && self.has_advisory_vetting() && self.age_cutoff.is_some()
    }

    /// Stable fragment for install-state lifecycle scheduling. If
    /// the floor's effective trust posture changes between installs,
    /// the delta build path must fall back to the full eligible scan
    /// so packages that just became trusted are not skipped merely
    /// because their package bytes are unchanged.
    ///
    /// `inherited` is deliberately NOT folded in. It is a per-run graph
    /// fact, not a posture: the set grows on every `add`, so folding it
    /// would churn this hash and force a full eligible scan on every
    /// install that touches the manifest. The one transition it can
    /// cause — a package denied for being too young, then waived once it
    /// is inherited — is already carried by
    /// `select_previously_unreviewed_now_allowed`, which reselects
    /// packages that were unreviewed last time and are allowed now.
    pub(crate) fn fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"default-trust-floor-v1");
        hasher.update(if self.enabled { b"\x01" } else { b"\x00" });
        hasher.update(if self.osv_gate_active {
            b"\x01"
        } else {
            b"\x00"
        });
        hasher.update(if self.lockfile_vetted {
            b"\x01"
        } else {
            b"\x00"
        });
        hasher.update(if self.age_cutoff.is_some() {
            b"\x01"
        } else {
            b"\x00"
        });
        hasher.finalize().to_hex().to_string()
    }

    /// The advisory-vetting precondition for trusting the allowlist:
    /// either an OSV `MAL-*` gate ran against this install's graph, or
    /// the graph was inherited from an unchanged lockfile that was
    /// advisory-checked when it was written. A frozen install
    /// (`aube ci`, `--frozen-lockfile`, a CI/teammate clone) correctly
    /// skips per-install OSV, so without the lockfile-vetting branch
    /// its trusted packages' build scripts would silently not run even
    /// though they ran for whoever locked the file
    /// (`wiki/commands/pm/supply-chain-posture.md` Decision 2).
    fn has_advisory_vetting(&self) -> bool {
        self.osv_gate_active || self.lockfile_vetted
    }

    /// Whether the floor trusts this resolved package. `times` is the
    /// graph's publish-time map (`LockfileGraph::times`).
    pub(crate) fn trusts(
        &self,
        pkg: &aube_lockfile::LockedPackage,
        times: &BTreeMap<String, String>,
    ) -> bool {
        self.has_advisory_vetting() && self.trusts_ignoring_vetting(pkg, times)
    }

    /// This install lacked advisory vetting, and that alone is why the
    /// floor did not trust the package — every other gate (enabled,
    /// registry provenance, the allowlist, the cooling window) holds.
    ///
    /// Vetting is the floor's one *run-scoped* input: it turns on the
    /// OSV gate reaching the network, or on the graph being inherited
    /// from an already-vetted lockfile. Two otherwise-identical runs can
    /// therefore disagree, and the run that says no leaves an allowed
    /// build unrun. The install records these so the freshness predicate
    /// can retry them instead of sealing the tree (nubjs/nub#764) — the
    /// config-derived gates are excluded on purpose, because a package
    /// the config denies is denied stably and re-running the full
    /// install could never change the answer.
    pub(crate) fn deferred_by_missing_vetting(
        &self,
        pkg: &aube_lockfile::LockedPackage,
        times: &BTreeMap<String, String>,
    ) -> bool {
        !self.has_advisory_vetting() && self.trusts_ignoring_vetting(pkg, times)
    }

    /// [`Self::trusts`] with the advisory-vetting gate assumed to hold.
    fn trusts_ignoring_vetting(
        &self,
        pkg: &aube_lockfile::LockedPackage,
        times: &BTreeMap<String, String>,
    ) -> bool {
        let Some(cutoff) = self.age_cutoff.as_deref() else {
            return false;
        };
        if !self.enabled {
            return false;
        }
        // Registry-resolved only. `local_source` covers file / link /
        // tarball / git / portal / exec; `registry_git_hosted` covers
        // registry-keyed entries third-party lockfiles mark as hosted
        // git. Match on `registry_name()` so an npm alias can't borrow
        // a listed name's trust (same rule as `BuildPolicy::decide`).
        if pkg.local_source.is_some() || pkg.registry_git_hosted {
            return false;
        }
        if !aube_scripts::is_default_trusted(pkg.registry_name()) {
            return false;
        }
        // Cooling window. The resolver keys fresh entries by dep_path
        // (peer suffix included); the pnpm lockfile round-trip re-keys
        // them as canonical `name@version`. Probe all spellings.
        let canonical = format!("{}@{}", pkg.registry_name(), pkg.version);
        let aliased = format!("{}@{}", pkg.name, pkg.version);
        let published = times
            .get(&canonical)
            .or_else(|| times.get(&aliased))
            .or_else(|| times.get(&pkg.dep_path));
        match published {
            // Known publish time: enforce the window on every install.
            Some(t) => t.as_str() <= cutoff,
            // Unknown publish time. For a NEW pick the resolver records
            // `time:` data in-memory whenever `minimumReleaseAge` is
            // active, so a missing time means the package never cleared
            // resolution-time vetting — fail closed. For an INHERITED
            // pick the version was already pinned and vetted when the
            // lockfile was written; the cooling window is a
            // *resolution-time* defense (don't pull a brand-new
            // version), and an inherited pick pulls nothing new. Since
            // `2b61eaa` the lockfile no longer persists `time:` data
            // under non-time-based resolution (upstream #892), so
            // `times` is legitimately empty for such picks — and
            // re-applying the age gate against that absence would
            // wrongly deny build scripts that ran for whoever locked the
            // file. The other gates (registry provenance, advisory
            // vetting, the allowlist) all still applied above, so
            // waiving *only* the cooling window here keeps the floor
            // sound.
            //
            // Per-package rather than the whole-graph `lockfile_vetted`:
            // see [`InheritedPicks`]. Reading the boolean here meant one
            // new pick anywhere in the graph revoked the waiver for
            // every unrelated inherited package, which is why `aube add`
            // un-trusted packages a plain `aube install` trusts.
            None => self.inherited.contains(&canonical),
        }
    }
}

/// The full precedence chain for one package: explicit policy first
/// (allows, denies, `dangerouslyAllowAllBuilds`), the floor on
/// `Unspecified`. Returns `Unspecified` when neither layer decides —
/// the caller's default-deny + unreviewed warning applies.
///
/// The explicit layer is consulted with the package's source and
/// git-repository approval keys, so a bare name approval cannot vouch
/// for `file:`/git/tarball-backed bytes — the same call
/// `run_dep_lifecycle_scripts` makes, which is what keeps the runner
/// and the unreviewed-builds warning from disagreeing.
pub(crate) fn decide_with_floor(
    policy: &aube_scripts::BuildPolicy,
    floor: &DefaultTrustFloor,
    pkg: &aube_lockfile::LockedPackage,
    times: &BTreeMap<String, String>,
) -> AllowDecision {
    let source_key = pkg.source_approval_key();
    let git_repository_key = pkg.git_repository_approval_key();
    match policy.decide_package_with_git_repository(
        pkg.registry_name(),
        &pkg.version,
        source_key.as_deref(),
        git_repository_key.as_deref(),
    ) {
        AllowDecision::Unspecified if floor.trusts(pkg, times) => AllowDecision::Allow,
        decision => decision,
    }
}

/// Whether dependency build scripts may run on this install — and thus
/// whether the per-dep `.bin` linking pass (`link_dep_bins`) must fire
/// so those scripts can call binaries declared in their own
/// `dependencies`.
///
/// Single source of truth for two call sites that previously open-coded
/// the predicate and drifted apart: the link phase (`link.rs`, the write
/// side that shims each dep's children into its `.bin`) and the
/// lifecycle phase (`finalize.rs`, the read side that runs the scripts
/// with those `.bin` dirs on PATH). They MUST agree — when they didn't,
/// a pure trust-floor install (no explicit `allowBuilds`) ran a dep's
/// postinstall but never linked the dep's own deps' bins, so a script
/// calling a dep-provided CLI (e.g. lmdb's
/// `node-gyp-build-optional-packages`) failed with exit 127.
///
/// `--ignore-scripts` skips scripts entirely, so it forces this off.
/// Otherwise scripts run when the policy has an explicit allow rule OR
/// the `defaultTrust` floor could authorize something.
pub(crate) fn dep_build_scripts_may_run(
    ignore_scripts: bool,
    has_any_allow_rule: bool,
    floor_may_allow_any: bool,
) -> bool {
    !ignore_scripts && (has_any_allow_rule || floor_may_allow_any)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_manifest::AllowBuildRaw;

    fn active_floor() -> DefaultTrustFloor {
        DefaultTrustFloor {
            enabled: true,
            osv_gate_active: true,
            lockfile_vetted: false,
            inherited: InheritedPicks::FromPriorLockfile(HashSet::new()),
            age_cutoff: aube_resolver::MinimumReleaseAge {
                minutes: 1440,
                ..Default::default()
            }
            .cutoff(),
        }
    }

    /// A frozen-install floor: OSV was (correctly) skipped this
    /// install — `osv_gate_active = false` — but the graph came from
    /// an unchanged lockfile, so it carries resolution-time vetting
    /// (`lockfile_vetted = true`). Models `aube ci`, `--frozen-lockfile`,
    /// and a CI/teammate clone.
    fn frozen_floor() -> DefaultTrustFloor {
        DefaultTrustFloor {
            osv_gate_active: false,
            lockfile_vetted: true,
            inherited: InheritedPicks::All,
            ..active_floor()
        }
    }

    /// The `aube add` shape: the resolver ran and produced new picks, so
    /// this install's advisory coverage comes from the live OSV gate
    /// (`osv_gate_active = true`, `lockfile_vetted = false`) — while
    /// every package listed in `inherited` was reproduced from the prior
    /// lockfile unchanged.
    fn add_floor(inherited: &[&str]) -> DefaultTrustFloor {
        DefaultTrustFloor {
            inherited: InheritedPicks::FromPriorLockfile(
                inherited.iter().map(|s| s.to_string()).collect(),
            ),
            ..active_floor()
        }
    }

    /// `esbuild` is on the vendored list; pinned by the aube-scripts
    /// loader test, relied on here.
    fn listed_pkg() -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: "esbuild".into(),
            version: "0.19.0".into(),
            dep_path: "esbuild@0.19.0".into(),
            ..Default::default()
        }
    }

    /// Publish-time map placing `pkg` `minutes` in the past, formatted
    /// exactly like the registry `time:` data the floor compares
    /// against. Reuses `MinimumReleaseAge::cutoff` ("now − minutes" as
    /// an ISO-8601 string) so the comparison shape matches production.
    fn times_published_minutes_ago(
        pkg: &aube_lockfile::LockedPackage,
        minutes: u64,
    ) -> BTreeMap<String, String> {
        let published = aube_resolver::MinimumReleaseAge {
            minutes,
            ..Default::default()
        }
        .cutoff()
        .expect("non-zero minutes always produce a cutoff");
        let mut times = BTreeMap::new();
        times.insert(format!("{}@{}", pkg.name, pkg.version), published);
        times
    }

    fn policy_from(pairs: &[(&str, bool)], dangerously: bool) -> aube_scripts::BuildPolicy {
        let map: BTreeMap<String, AllowBuildRaw> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), AllowBuildRaw::Bool(*v)))
            .collect();
        let (p, errs) = aube_scripts::BuildPolicy::from_config(&map, &[], &[], dangerously);
        assert!(errs.is_empty(), "unexpected policy warnings: {errs:?}");
        p
    }

    #[test]
    fn floor_allows_an_aged_registry_package_the_policy_left_unspecified() {
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);
        let decision = decide_with_floor(&policy_from(&[], false), &active_floor(), &pkg, &times);
        assert_eq!(decision, AllowDecision::Allow);
    }

    #[test]
    fn explicit_false_carves_a_package_out_of_the_floor() {
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);
        let policy = policy_from(&[("esbuild", false)], false);
        assert_eq!(
            decide_with_floor(&policy, &active_floor(), &pkg, &times),
            AllowDecision::Deny,
            "an explicit allowBuilds false must beat the floor"
        );
    }

    #[test]
    fn allow_builds_existence_does_not_disable_the_floor() {
        // The map naming an unrelated package must not flip the floor
        // off for everything else — approve-builds writes the map on
        // first approval, and existence-toggling would revoke
        // unrelated packages' trust.
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);
        let policy = policy_from(&[("some-other-pkg", false)], false);
        assert_eq!(
            decide_with_floor(&policy, &active_floor(), &pkg, &times),
            AllowDecision::Allow
        );
    }

    #[test]
    fn floor_ignores_git_file_and_tarball_sources() {
        let times = times_published_minutes_ago(&listed_pkg(), 10 * 1440);
        let floor = active_floor();
        let mut git = listed_pkg();
        git.local_source = Some(aube_lockfile::LocalSource::Git(aube_lockfile::GitSource {
            url: "https://github.com/evil/esbuild.git".into(),
            committish: None,
            resolved: "0123456789012345678901234567890123456789".into(),
            integrity: None,
            subpath: None,
        }));
        assert!(!floor.trusts(&git, &times), "git source must never floor");
        let mut tarball = listed_pkg();
        tarball.local_source = Some(aube_lockfile::LocalSource::Tarball("./esbuild.tgz".into()));
        assert!(
            !floor.trusts(&tarball, &times),
            "file tarball must never floor"
        );
        let mut hosted = listed_pkg();
        hosted.registry_git_hosted = true;
        assert!(
            !floor.trusts(&hosted, &times),
            "registry-keyed hosted git must never floor"
        );
    }

    #[test]
    fn floor_refuses_an_alias_borrowing_a_listed_name() {
        let times = times_published_minutes_ago(&listed_pkg(), 10 * 1440);
        let mut aliased = listed_pkg();
        aliased.name = "esbuild".into();
        aliased.alias_of = Some("evil-pkg".into());
        assert!(
            !active_floor().trusts(&aliased, &times),
            "trust must key off the registry name, not the in-tree alias"
        );
    }

    /// A run with no advisory vetting skips the whole dep-script phase,
    /// so a package the floor would otherwise have built goes unbuilt —
    /// and before nubjs/nub#764 nothing recorded that, which sealed the
    /// tree. Only the vetting gate may produce a deferral: every other
    /// gate is config-derived, so a package it denies is denied stably
    /// and re-running the install could not change the answer.
    #[test]
    fn missing_vetting_defers_a_build_that_every_other_gate_allows() {
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);

        let mut no_vetting = active_floor();
        no_vetting.osv_gate_active = false;
        assert!(
            no_vetting.deferred_by_missing_vetting(&pkg, &times),
            "vetting is the only gate that failed, so this build is owed a retry"
        );

        assert!(
            !active_floor().deferred_by_missing_vetting(&pkg, &times),
            "a run that DID build the package owes nothing"
        );
        assert!(
            !DefaultTrustFloor::disabled().deferred_by_missing_vetting(&pkg, &times),
            "defaultTrust=false is a stable config denial, not a deferral"
        );

        // Config-derived denials stay out of the deferred set even when
        // vetting is also missing — otherwise every install would bust
        // the warm path for a package that can never become allowed.
        let mut unlisted = listed_pkg();
        unlisted.name = "not-on-the-trust-list".into();
        assert!(
            !no_vetting.deferred_by_missing_vetting(&unlisted, &times),
            "a package off the allowlist is denied stably"
        );
        let young = times_published_minutes_ago(&pkg, 60);
        assert!(
            !no_vetting.deferred_by_missing_vetting(&pkg, &young),
            "a version inside the cooling window is denied by config, not by the run"
        );
        assert!(
            !no_vetting.deferred_by_missing_vetting(&pkg, &BTreeMap::new()),
            "an unknown publish time fails closed rather than deferring forever"
        );
    }

    #[test]
    fn floor_defers_to_the_osv_gate_and_the_off_switch() {
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);
        // No OSV *and* no lockfile vetting (a fresh resolve where every
        // advisory backend is off) → the floor has no resolution-time
        // vetting to inherit, so it stays closed.
        let mut no_vetting = active_floor();
        no_vetting.osv_gate_active = false;
        assert!(
            !no_vetting.trusts(&pkg, &times),
            "no OSV coverage and no lockfile vetting → no floor"
        );
        assert!(!no_vetting.may_allow_any());
        let off = DefaultTrustFloor::disabled();
        assert!(!off.trusts(&pkg, &times), "defaultTrust=false → no floor");
        assert!(!off.may_allow_any());
    }

    /// THE BUG FIX (supply-chain-posture.md Decision 2): a frozen
    /// install correctly skips per-install OSV, so `osv_gate_active`
    /// is false — yet a trusted package's build scripts must still run,
    /// inheriting the resolution-time vetting baked into the lockfile
    /// (registry provenance + the cooling window against the lockfile's
    /// recorded publish time). Before the fix the floor hard-required
    /// `osv_gate_active`, so CI/clones silently skipped esbuild /
    /// better-sqlite3 / node-gyp builds that ran for whoever locked.
    #[test]
    fn frozen_install_trusts_the_allowlist_by_inheriting_lockfile_vetting() {
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);
        let floor = frozen_floor();
        assert!(
            floor.may_allow_any(),
            "a frozen install with lockfile vetting must keep the dep-script phase alive"
        );
        assert_eq!(
            decide_with_floor(&policy_from(&[], false), &floor, &pkg, &times),
            AllowDecision::Allow,
            "a frozen install must run a trusted package's build script"
        );
    }

    /// Frozen install + a KNOWN publish time still enforces the cooling
    /// window: a too-young pinned version must not floor. The window is
    /// only waived when the time is *absent* (see the next test), never
    /// overridden when it is present and fails the gate.
    #[test]
    fn frozen_install_still_enforces_the_window_when_the_time_is_known() {
        let pkg = listed_pkg();
        let floor = frozen_floor();
        let young = times_published_minutes_ago(&pkg, 60);
        assert!(
            !floor.trusts(&pkg, &young),
            "a too-young version with a known publish time must not floor, even frozen"
        );
    }

    /// Frozen install + an UNKNOWN publish time waives the cooling
    /// window. The lockfile no longer persists `time:` data under
    /// non-time-based resolution (upstream #892, commit `2b61eaa`), so a
    /// frozen reinstall legitimately has empty `times`. The cooling
    /// window is a resolution-time defense and a frozen install pulls
    /// nothing new, so the floor inherits the lockfile's vetting and
    /// trusts the (registry-only, allowlisted, advisory-vetted) package.
    /// A FRESH install with the same empty map still fails closed — the
    /// waiver is gated on `lockfile_vetted`.
    #[test]
    fn frozen_install_waives_the_window_when_the_publish_time_is_unknown() {
        let pkg = listed_pkg();
        assert!(
            frozen_floor().trusts(&pkg, &BTreeMap::new()),
            "a frozen install must trust an allowlisted package when the lockfile \
             carries no publish time (post-#892 non-time-based lockfiles)"
        );
        assert!(
            !active_floor().trusts(&pkg, &BTreeMap::new()),
            "a NEW pick with no recorded publish time must still fail closed — \
             the waiver is gated on the pick being inherited"
        );
    }

    /// `aube add <something-else>` must not un-trust an unrelated
    /// package that was already in the lockfile and already built
    /// (nubjs/nub#764).
    ///
    /// The add re-resolves, so `lockfile_vetted` goes false for the
    /// whole graph — but the graph is a MIXTURE: only the added package
    /// is new. Reading that one boolean in the cooling window's
    /// unknown-time arm revoked the waiver for every inherited package
    /// too, so the same tree, config and package flipped from trusted to
    /// unreviewed on `add` and back on the next `install`.
    ///
    /// Nothing here loosens the floor: the package this admits was
    /// already built by the install that locked it, its version is
    /// unchanged, and this install's live OSV gate covers it. The
    /// widening is against the *previous behavior*, not against the
    /// documented posture — which is what makes it the bug.
    #[test]
    fn adding_a_dependency_does_not_untrust_the_packages_already_locked() {
        let pkg = listed_pkg();
        let inherited_key = format!("{}@{}", pkg.name, pkg.version);
        // Post-#892 the reproduced pick carries no publish time.
        let no_times = BTreeMap::new();

        assert!(
            add_floor(&[&inherited_key]).trusts(&pkg, &no_times),
            "a pick reproduced unchanged from the lockfile keeps its waiver when an \
             unrelated package is added"
        );
        assert!(
            !add_floor(&[]).trusts(&pkg, &no_times),
            "the package being ADDED is a new pick with no publish time, so it still \
             fails closed — the waiver did not become unconditional"
        );
        assert!(
            !add_floor(&[&inherited_key]).trusts(&pkg, &times_published_minutes_ago(&pkg, 60)),
            "a KNOWN too-young publish time still denies an inherited pick: only the \
             absence of a time is waived"
        );
        // The deferral marker exists for builds a run could not attempt.
        // An inherited pick that this run trusts was attempted, so it
        // owes nothing — otherwise every `add` would strand a retry.
        assert!(
            !add_floor(&[&inherited_key]).deferred_by_missing_vetting(&pkg, &no_times),
            "a package this run trusted and built is not owed a retry"
        );
    }

    #[test]
    fn floor_requires_the_cooling_window_per_version() {
        let pkg = listed_pkg();
        let floor = active_floor();
        let young = times_published_minutes_ago(&pkg, 60);
        assert!(
            !floor.trusts(&pkg, &young),
            "a version younger than minimumReleaseAge must not floor"
        );
        assert!(
            !floor.trusts(&pkg, &BTreeMap::new()),
            "unknown publish time must fail closed"
        );
        let mut no_window = active_floor();
        no_window.age_cutoff = None;
        let aged = times_published_minutes_ago(&pkg, 10 * 1440);
        assert!(
            !no_window.trusts(&pkg, &aged),
            "minimumReleaseAge=0 must disable the floor"
        );
    }

    #[test]
    fn explicit_false_survives_dangerously_allow_all_when_the_floor_is_active() {
        // The documented chain when defaultTrust is on: explicit entry
        // > allow-all > floor. The install path composes this via
        // `allow_all_except_denied`; pin the end-to-end decision here.
        let pkg = listed_pkg();
        let times = times_published_minutes_ago(&pkg, 10 * 1440);
        let policy = policy_from(&[("esbuild", false)], false).allow_all_except_denied();
        assert_eq!(
            decide_with_floor(&policy, &active_floor(), &pkg, &times),
            AllowDecision::Deny
        );
        let other = aube_lockfile::LockedPackage {
            name: "anything-else".into(),
            version: "1.0.0".into(),
            dep_path: "anything-else@1.0.0".into(),
            ..Default::default()
        };
        assert_eq!(
            decide_with_floor(&policy, &active_floor(), &other, &times),
            AllowDecision::Allow,
            "allow-all still wins above the floor for everything not denied"
        );
    }

    /// THE TRANSITIVE-BIN FIX: the per-dep `.bin` linking pass
    /// (`link.rs`'s `link_dep_bins`) and the lifecycle-script phase
    /// (`finalize.rs`'s `run_dep_lifecycle_scripts`) MUST gate on the
    /// same predicate. They drifted: bin-linking checked only
    /// `has_any_allow_rule`, while scripts also ran on the
    /// `defaultTrust` floor. So on a pure trust-floor install (no
    /// explicit `allowBuilds`) a dep's postinstall ran but its own
    /// deps' bins were never shimmed onto PATH — a script calling a
    /// dep-provided CLI (lmdb's `node-gyp-build-optional-packages`)
    /// died with exit 127. `dep_build_scripts_may_run` is now the
    /// single source of truth both sites consume.
    #[test]
    fn bin_linking_gate_fires_on_a_pure_trust_floor_install() {
        // No allow rule, but the floor could authorize a build (the
        // exact lmdb/Gatsby shape). Scripts will run, so the bins
        // their scripts need MUST be linked.
        assert!(
            dep_build_scripts_may_run(false, false, true),
            "trust-floor-only install must still link dep bins — \
             scripts run on the floor and need their own deps' CLIs on PATH"
        );
        // Symmetric: an explicit allow rule with the floor closed still fires.
        assert!(dep_build_scripts_may_run(false, true, false));
        // Both off → nothing to run, skip the pass (fast path preserved).
        assert!(!dep_build_scripts_may_run(false, false, false));
        // `--ignore-scripts` forces the whole thing off regardless.
        assert!(!dep_build_scripts_may_run(true, true, true));
    }
}
