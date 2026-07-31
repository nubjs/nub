//! The build jail's DEV-ONLY catalog override: read `build-jail-catalog.json` from a path at
//! startup instead of the copy baked in at compile time.
//!
//! WHY THIS EXISTS AT ALL. The catalog is codegen'd into the binary by `build.rs`, so every
//! edit — one sibling directory, one promoted `$downloads` host — costs a full Rust rebuild.
//! That is the whole inner loop of a corpus run: install ~344 packages under the jail, watch
//! one fail, add its grant, re-run. Compiling between every iteration dominates the loop, and
//! nothing about the experiment needs a fresh binary. This seam turns each iteration into an
//! edit and a re-run.
//!
//! THE PROPERTIES THAT MAKE IT SAFE, in the order they matter:
//!
//! 1. **It is compile-time gated OFF, and the code is ABSENT rather than inert.** The loader
//!    lives behind the `build-jail-catalog-override` cargo feature, which no shipped build
//!    enables — the same shape as [`crate::arm`] and nub-cli's `prefetch-github-hosts`.
//!    Without the feature `crate::catalog` is not compiled into the crate at all, so a release
//!    binary contains no catalog parser and no path by which one could run. An env var alone
//!    would be one stray export away from a user pointing the jail at an attacker's file, which
//!    is precisely the remote-authority hazard `data/README.md` argues against.
//! 2. **A set variable a build cannot honour is a hard REFUSAL, not a no-op.** The name is
//!    recognized in every build; only the behaviour is gated. This is [`crate::arm`]'s
//!    anti-hollow rule and it exists because a corpus run once measured two arms that were
//!    secretly the same configuration. A harness pointing at a catalog the binary ignores must
//!    find out immediately.
//! 3. **It runs the SAME validator as the build.** [`crate::catalog::parse`] is one
//!    implementation shared with `build.rs` (see that module's docs), so an override cannot
//!    introduce a shape the compiled path would have rejected — no traversing project read, no
//!    wildcard host, no unprovenanced entry. It supplies DATA for the existing compiled-in
//!    shapes; it cannot add a new KIND of grant or alter the authorship key, which stays
//!    aube's installer-resolved name.
//! 4. **Every failure falls back to the compiled catalog, loudly.** A missing file, bad JSON,
//!    or a validation rejection resolves to the baked-in copy — never to "grant everything",
//!    and never silently. `data/README.md` is explicit that the compiled copy is the FLOOR and
//!    that a verification failure must be logged, "because it is indistinguishable from an
//!    attack". Falling back rather than aborting is deliberate: a typo in a scratch catalog
//!    should not take down a 344-package run, and the banner is what keeps it visible.
//!
//! It REPLACES the compiled catalog rather than merging over it. A merge would leave every
//! iteration asking which copy won a given package — and since the corpus loop edits a copy OF
//! the real catalog, replacement is also what makes the file on disk mean exactly what it says.

/// The env var naming the catalog to load. INTERNAL PLUMBING, never a documented user knob:
/// it names a dev-only seam that exists in no shipped build.
pub const CATALOG_ENV: &str = "NUB_BUILD_JAIL_CATALOG";

/// The name a refusal points the reader at.
const FEATURE: &str = "build-jail-catalog-override";

/// What startup learned from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The variable is unset: the seam is entirely inert, as in every shipped build, and the
    /// compiled catalog is used.
    Default,
    /// A catalog was loaded and REPLACES the compiled one.
    Loaded { path: String, summary: String },
    /// The override could not be used and the compiled catalog stands. Carries the reason so
    /// the banner can state it — a silent fallback is the one outcome this seam must not have.
    FellBack { path: String, reason: String },
}

impl Decision {
    /// The line a run greps for to prove which catalog is in force. `None` on the default
    /// path, where there is nothing to assert.
    pub fn banner(&self) -> Option<String> {
        match self {
            Decision::Default => None,
            Decision::Loaded { path, summary } => Some(format!(
                "warning: build-jail catalog OVERRIDDEN from {path} ({summary}) \
                 — development-only, not a shipped configuration"
            )),
            Decision::FellBack { path, reason } => Some(format!(
                "warning: build-jail catalog override at {path} was REJECTED ({reason}); \
                 using the compiled-in catalog"
            )),
        }
    }
}

/// Read the environment once and latch the answer. Called from nub's startup, before the
/// build jail is installed and long before any dependency script exists to influence it.
///
/// `Err` is a REFUSAL the caller must abort on — a set variable that a binary cannot honour
/// must never degrade to the compiled catalog silently.
pub fn init_from_env() -> Result<Decision, String> {
    decide(std::env::var(CATALOG_ENV).ok().as_deref())
}

/// [`init_from_env`] with the variable injected, so every arm is testable without mutating
/// process state.
pub fn decide(value: Option<&str>) -> Result<Decision, String> {
    let Some(path) = value else {
        return Ok(Decision::Default);
    };
    if path.trim().is_empty() {
        return Err(format!(
            "{CATALOG_ENV} is set to an empty value; unset it or name a catalog file."
        ));
    }
    if !cfg!(feature = "build-jail-catalog-override") {
        return Err(format!(
            "{CATALOG_ENV} is set, but this binary was not built with the `{FEATURE}` \
             feature, so it cannot honour it.\n  \
             Refusing rather than running the compiled-in catalog under an override's name.\n  \
             Rebuild with `--features nub-cli/{FEATURE}`, or unset the variable."
        ));
    }
    Ok(load(path))
}

#[cfg(feature = "build-jail-catalog-override")]
mod loader {
    use super::Decision;
    use crate::catalog::Catalog;
    use std::sync::OnceLock;

    /// The latched override. `None` means the compiled catalog stands — the state of every
    /// build that never opts in, and of one whose override was rejected.
    static LOADED: OnceLock<Option<&'static Catalog>> = OnceLock::new();

    pub(crate) fn load(path: &str) -> Decision {
        // AT MOST ONCE per process. Startup calls this exactly once; a second call must not
        // report a fresh parse as the catalog in force, because the FIRST latch is what every
        // consumer already holds. Reporting the fresh parse would be the same silent
        // wrong-answer this whole seam exists to prevent.
        if LOADED.get().is_some() {
            return Decision::FellBack {
                path: path.to_string(),
                reason: "a catalog was already latched earlier in this process".to_string(),
            };
        }

        let outcome = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read: {e}"))
            .and_then(|text| crate::catalog::parse(&text));

        match outcome {
            Ok(catalog) => {
                let summary = format!(
                    "{} hosts, {} package grants, {} egress entries",
                    catalog.download_hosts.len(),
                    catalog.package_grants.len(),
                    catalog.package_network_allowed.len()
                );
                // Leaked deliberately: the compiled tables are `&'static` and every consumer
                // holds them that way, so a `'static` override keeps ALL downstream types and
                // signatures identical. This runs at most once per process, before any jail is
                // compiled — it is a load, not a leak in the growing sense.
                let _ = LOADED.set(Some(Box::leak(Box::new(catalog))));
                Decision::Loaded {
                    path: path.to_string(),
                    summary,
                }
            }
            Err(reason) => {
                let _ = LOADED.set(None);
                Decision::FellBack {
                    path: path.to_string(),
                    reason,
                }
            }
        }
    }

    pub(crate) fn active() -> Option<&'static Catalog> {
        *LOADED.get().unwrap_or(&None)
    }
}

#[cfg(feature = "build-jail-catalog-override")]
use loader::{active, load};

/// Without the feature there is no loader, no parser, and no path that could produce one —
/// [`decide`] refuses any set variable before reaching here.
#[cfg(not(feature = "build-jail-catalog-override"))]
fn load(_path: &str) -> Decision {
    unreachable!("decide() refuses the variable before loading in a non-featured build")
}

/// The overriding `$downloads` host list, or `None` for the compiled one.
pub(crate) fn download_hosts() -> Option<&'static [&'static str]> {
    #[cfg(feature = "build-jail-catalog-override")]
    {
        static HOSTS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
        let catalog = active()?;
        Some(HOSTS.get_or_init(|| catalog.download_hosts.iter().map(String::as_str).collect()))
    }
    #[cfg(not(feature = "build-jail-catalog-override"))]
    {
        None
    }
}

/// The overriding per-package egress set — each name with the semver range its grant is
/// scoped to, `None` meaning every version. Sorted by name by the parser, which is what keeps
/// the caller's `binary_search_by` correct.
pub(crate) fn package_network_allowed() -> Option<&'static [(&'static str, Option<&'static str>)]> {
    #[cfg(feature = "build-jail-catalog-override")]
    {
        static ALLOWED: std::sync::OnceLock<Vec<(&'static str, Option<&'static str>)>> =
            std::sync::OnceLock::new();
        let catalog = active()?;
        Some(ALLOWED.get_or_init(|| {
            catalog
                .package_network_allowed
                .iter()
                .map(|g| (g.package.as_str(), g.versions.as_deref()))
                .collect()
        }))
    }
    #[cfg(not(feature = "build-jail-catalog-override"))]
    {
        None
    }
}

/// The overriding per-package grant entries, or `None`. Returned as the parsed shape rather
/// than the compiler's `CuratedGrant` because that type is private to
/// [`crate::compiler::curated`], which owns the conversion.
#[cfg(feature = "build-jail-catalog-override")]
pub(crate) fn package_grants() -> Option<&'static [crate::catalog::PackageGrant]> {
    Some(active()?.package_grants.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inert-by-default half. `decide(None)` is the shipped path in EVERY build, featured
    /// or not, and no table is overridden before a latch — which is what makes the seam
    /// unreachable rather than merely unused.
    #[test]
    fn unset_is_inert_in_every_build() {
        assert_eq!(decide(None), Ok(Decision::Default));
        assert_eq!(Decision::Default.banner(), None);
        assert_eq!(download_hosts(), None);
        assert_eq!(package_network_allowed(), None);
    }

    /// The anti-hollow half, and the only assertion that differs by build. A default build
    /// must REFUSE the variable so a harness cannot iterate against a catalog the binary is
    /// silently ignoring.
    #[test]
    fn a_set_variable_is_never_a_silent_no_op() {
        let accepted = decide(Some("/nonexistent/catalog.json"));
        if cfg!(feature = "build-jail-catalog-override") {
            // A path that cannot be read must fall back rather than abort: a typo in a
            // scratch catalog should not take down a 344-package corpus run.
            let decision = accepted.expect("a featured build falls back, it does not refuse");
            assert!(
                matches!(decision, Decision::FellBack { .. }),
                "an unreadable override must fall back to the compiled catalog: {decision:?}"
            );
            assert!(
                decision.banner().is_some_and(|b| b.contains("compiled-in")),
                "the fallback must announce itself — a silent one is indistinguishable from \
                 an attack"
            );
        } else {
            let err = accepted.expect_err("a default build must refuse, not ignore");
            assert!(
                err.contains(FEATURE),
                "the refusal must name the fix: {err}"
            );
        }
        // An empty value is refused in both builds: it is always a harness bug.
        assert!(decide(Some("")).is_err());
    }
}
