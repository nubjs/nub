//! The build jail's one TEST-ONLY experimental seam: dropping the curated
//! project-file read grant so the corpus study's project-grant arm is a runtime
//! setting rather than a patched binary.
//!
//! WHY THIS EXISTS AT ALL. The corpus study measures which dependencies genuinely
//! need the consumer's project directory, and the honest experiment removes the
//! grant and observes what breaks. The grant is a `vec!` literal in
//! [`crate::compiler::preset::grant_build_jail_dependency_reads`], so without a seam
//! that arm costs one patched build per shard instead of one env setting per run.
//!
//! THE THREE PROPERTIES THAT MAKE IT SAFE, in the order they matter:
//!
//! 1. **It carries no path channel.** The seam is a BOOLEAN that DROPS a nub-curated
//!    set. There is no spelling of it that adds a root, names a package, or accepts a
//!    path — so the allowlist's authorship invariant (nub-curated, never
//!    dependency-authored) is preserved structurally, not by validation. The only
//!    reachable effect is strictly LESS access than the default build.
//! 2. **It is compile-time gated OFF.** The behaviour lives behind the
//!    `build-jail-project-grant-arm` cargo feature, which no shipped build enables —
//!    the same reasoning, and the same shape, as `prefetch-github-hosts`. An env var
//!    alone would be one stray export away from a user or a CI job silently changing
//!    enforcement.
//! 3. **It latches ONCE, before any dependency code runs.** [`init_from_env`] is
//!    called from nub's startup and the answer is frozen in a `OnceLock`. A lifecycle
//!    script cannot mutate nub's own environment, and even a hypothetical route that
//!    could would arrive after the latch.
//!
//! THE ANTI-HOLLOW GUARANTEE, which is the point of the preflight. A prior corpus
//! run selected its arms with an env var that the binary under test did not read: both
//! arms ran the identical configuration and a "jail off" control silently measured jail
//! on. So this seam is designed to make that failure IMPOSSIBLE rather than merely
//! unlikely, in two directions:
//!
//! - Setting the variable against a binary WITHOUT the feature is a hard startup
//!   REFUSAL, not a no-op. The name is recognized in every build; only the behaviour is
//!   gated. A run cannot silently get the default arm while believing otherwise.
//! - When the arm IS active the binary says so on stderr ([`Decision::banner`]), so the
//!   harness asserts the arm took effect from the run's own output — the same
//!   arm-effect assertion the study already applies to the per-package opt-out warning.

use std::sync::OnceLock;

/// The env var that selects the arm. INTERNAL PLUMBING, never a documented user knob:
/// it names a test-only seam that exists in no shipped build.
pub const PROJECT_GRANT_ENV: &str = "NUB_BUILD_JAIL_PROJECT_GRANT";

/// The one accepted value. The default build is the grant-ON arm, so `on` has no
/// spelling here — an arm that changes nothing would be a way to believe a seam fired
/// when it did not.
const OFF: &str = "off";

/// The name a refusal points the reader at.
const FEATURE: &str = "build-jail-project-grant-arm";

static PROJECT_GRANT_DROPPED: OnceLock<bool> = OnceLock::new();

/// What startup learned from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The variable is unset: the seam is entirely inert, as in every shipped build.
    Default,
    /// The arm is active. The caller MUST surface [`Decision::banner`] so a run can
    /// assert the arm took effect from its own output.
    ProjectGrantDropped,
}

impl Decision {
    /// The line a run greps for to prove the arm engaged. `None` on the default path,
    /// where there is nothing to assert.
    pub fn banner(self) -> Option<String> {
        match self {
            Decision::Default => None,
            Decision::ProjectGrantDropped => Some(format!(
                "warning: build-jail project read grant DROPPED ({PROJECT_GRANT_ENV}={OFF}) \
                 — experimental arm, not a shipped configuration"
            )),
        }
    }
}

/// Read the environment once and latch the answer. Called from nub's startup, before
/// the build jail is installed and long before any dependency script exists.
///
/// `Err` is a REFUSAL the caller must abort on — see the module doc: a set variable
/// that a binary cannot honour must never degrade to the default arm silently.
pub fn init_from_env() -> Result<Decision, String> {
    let decision = decide(std::env::var(PROJECT_GRANT_ENV).ok().as_deref())?;
    let _ = PROJECT_GRANT_DROPPED.set(decision == Decision::ProjectGrantDropped);
    Ok(decision)
}

/// [`init_from_env`] with the variable injected, so both refusal arms are testable
/// without mutating process state.
pub fn decide(value: Option<&str>) -> Result<Decision, String> {
    let Some(value) = value else {
        return Ok(Decision::Default);
    };
    if !cfg!(feature = "build-jail-project-grant-arm") {
        return Err(format!(
            "{PROJECT_GRANT_ENV} is set, but this binary was not built with the \
             `{FEATURE}` feature, so it cannot honour it.\n  \
             Refusing rather than running the default configuration under an \
             experimental arm's name.\n  \
             Rebuild with `--features nub-cli/{FEATURE}`, or unset the variable."
        ));
    }
    if value == OFF {
        Ok(Decision::ProjectGrantDropped)
    } else {
        Err(format!(
            "{PROJECT_GRANT_ENV}={value} is not a value this seam accepts \
             (the only one is `{OFF}`; the default build is the grant-on arm)."
        ))
    }
}

/// Latch the arm without an environment round-trip, for the differential test that has
/// to compile a policy both ways in one process. Compiled ONLY in a featured build, so
/// the default build has no route to the latch at all — not even a test one.
#[cfg(feature = "build-jail-project-grant-arm")]
pub fn latch_for_test(dropped: bool) {
    let _ = PROJECT_GRANT_DROPPED.set(dropped);
}

/// Whether the curated project-file read grant is withheld. `false` — the shipped
/// behaviour — whenever [`init_from_env`] has not run, so a library embedder that
/// never opts in gets the default policy.
pub(crate) fn project_grant_dropped() -> bool {
    cfg!(feature = "build-jail-project-grant-arm") && *PROJECT_GRANT_DROPPED.get().unwrap_or(&false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inert-by-default half. `decide(None)` is the shipped path in EVERY build,
    /// featured or not, and `project_grant_dropped()` is false before any latch — which
    /// is what makes the seam unreachable rather than merely unused.
    #[test]
    fn unset_is_inert_in_every_build() {
        assert_eq!(decide(None), Ok(Decision::Default));
        assert_eq!(Decision::Default.banner(), None);
        assert!(!project_grant_dropped());
    }

    /// The anti-hollow half, and the only assertion that differs by build. A default
    /// build must REFUSE the variable (so a harness cannot run an arm that does
    /// nothing); a featured build must accept `off` and hand back a banner to assert on.
    #[test]
    fn a_set_variable_is_never_a_silent_no_op() {
        let accepted = decide(Some(OFF));
        if cfg!(feature = "build-jail-project-grant-arm") {
            assert_eq!(accepted, Ok(Decision::ProjectGrantDropped));
            assert!(
                Decision::ProjectGrantDropped
                    .banner()
                    .is_some_and(|b| b.contains(PROJECT_GRANT_ENV)),
                "the active arm must announce itself so a run can assert it took effect"
            );
        } else {
            let err = accepted.expect_err("a default build must refuse, not ignore");
            assert!(
                err.contains(FEATURE),
                "the refusal must name the fix: {err}"
            );
        }
        // An unrecognized value is refused in both builds, for the same reason.
        assert!(decide(Some("1")).is_err());
    }
}
