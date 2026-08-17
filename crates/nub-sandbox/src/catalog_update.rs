//! The SHIPPED catalog-update path: read a newer catalog from nub's own data directory, so a grant
//! fix does not require a nub release.
//!
//! WHY THIS IS NECESSARY AND NOT A CONVENIENCE. The catalog is `include_str!`-compiled, npm publishes
//! continuously, and the dev override ([`crate::catalog_override`]) is compiled OUT of every shipped
//! build. So today a package measured after a release is uncatalogued until the next release, by
//! construction, and the gap regenerates every week. Default-on confinement cannot depend on a release
//! cadence to stay correct.
//!
//! ⛔⛔ HOW THIS DIFFERS FROM THE DEV OVERRIDE, WHICH IS THE WHOLE SAFETY ARGUMENT. That seam reads a
//! path from an ENV VAR and is therefore feature-gated out of shipped builds: an env var is one stray
//! export away from pointing the jail at an attacker's file. This one takes NO path from the caller.
//! The location is fixed, computed from nub's own data directory, and there is no knob — not an env
//! var, not a config field, not a flag — that can redirect it. That is what makes it shippable.
//!
//! ⛔ OFFLINE-FIRST, AND THIS MODULE PERFORMS NO NETWORK I/O AT ALL. It reads a file if one is there.
//! Placing that file is a separate, explicit act; nothing here fetches, and an install with no
//! network behaves exactly as it does today. A catalog fetched during install would make the jail's
//! configuration depend on a reachable server at the worst possible moment.
//!
//! ⛔ THE COMPILED CATALOG IS THE FLOOR AND EVERY FAILURE FALLS BACK TO IT, LOUDLY. Missing file, bad
//! JSON, a validation rejection, an older stamp — all resolve to the baked copy with a stated reason.
//! Never to "grant everything", and never silently: `data/README.md` requires a verification failure
//! be logged "because it is indistinguishable from an attack".
//!
//! ⛔ IT RUNS THE SAME VALIDATOR AS THE BUILD. [`crate::catalog_v2::parse`] is one implementation
//! shared with `build.rs`, so an updated catalog cannot introduce a shape the compiled path would have
//! rejected. It supplies DATA for shapes that already exist; it cannot add a new KIND of grant.
//!
//! ⛔ IT REPLACES THE COMPILED CATALOG RATHER THAN MERGING OVER IT, so AN UPDATE MUST BE COMPLETE. A
//! file with three entries does not add three grants — it becomes the whole catalog, and every other
//! package drops to the baseline. That is the same rule the dev override follows, for the same reason:
//! a merge leaves every reader asking which copy won a given package. It is safe here because the only
//! thing that produces these files is the corpus collator, which emits the entire catalog. Verified by
//! running it: a one-package update reports `1 packages` in its banner rather than 295.
//!
//! ## The residual risk, stated rather than implied
//!
//! This file is writable by the user, and therefore by a build script the user has already granted
//! whole-disk write — 62 catalog entries carry one. Such a script could widen the catalog for OTHER
//! packages, persistently.
//!
//! That is a real residual and it is accepted for now, for a specific reason: a script with whole-disk
//! write can already plant a shell profile and obtain arbitrary code execution on the next shell, so
//! the boundary was crossed before the catalog was reachable. Rewriting the catalog is lateral and
//! persistent rather than an escalation of privilege. What closes it properly is SIGNING — verify a
//! detached signature against a key compiled into the binary, so a tampered file falls back to the
//! floor. That is deliberately not in this change: a signing key is a published, externally-committing
//! artifact, and the stopgap is useful without it.

use std::path::{Path, PathBuf};

/// The file name inside nub's data directory. Fixed, and not derived from any input.
const CATALOG_FILE: &str = "build-jail-catalog-v2.json";

/// The subdirectory holding it, so the data dir does not accumulate loose files.
const CATALOG_DIR: &str = "catalog";

/// What startup learned from disk. Mirrors [`crate::catalog_override::Decision`] deliberately: two
/// tiers that can both replace the compiled catalog should report themselves the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No file at the trusted location. The overwhelmingly common case, and entirely silent — a
    /// user who has never updated their catalog must not be told about it on every install.
    Absent,
    /// A newer catalog was loaded and REPLACES the compiled one.
    Loaded {
        path: String,
        generated_at: String,
        packages: usize,
    },
    /// A file was present and is not in force. Carries the reason, because a silent fallback is the
    /// one outcome this seam must not have.
    FellBack { path: String, reason: String },
}

impl Decision {
    /// The line a run can grep to prove which catalog is in force, or `None` when there is nothing
    /// to say. `Absent` is deliberately silent; `FellBack` never is.
    pub fn banner(&self) -> Option<String> {
        match self {
            Decision::Absent => None,
            Decision::Loaded {
                path,
                generated_at,
                packages,
            } => Some(format!(
                "build-jail catalog updated from {path} ({packages} packages, generated {generated_at})"
            )),
            Decision::FellBack { path, reason } => Some(format!(
                "warning: ignoring the build-jail catalog at {path} ({reason}) \
                 — the catalog compiled into this nub is in force"
            )),
        }
    }
}

/// Where an updated catalog is read from, or `None` when no data directory can be determined.
///
/// ⛔ NOT CONFIGURABLE, AND THAT IS THE POINT. The dev override takes its path from an env var, which
/// is exactly why it cannot ship. This resolves from the platform data directory and nothing else, so
/// there is no input — env, config, flag or argument — that can aim it somewhere else.
pub fn catalog_path(data_dir: Option<&Path>) -> Option<PathBuf> {
    Some(data_dir?.join(CATALOG_DIR).join(CATALOG_FILE))
}

/// Is `candidate` strictly newer than `baked`?
///
/// ⛔ STRICTLY, AND `None` LOSES. Equal stamps mean the same catalog, so replacing one with the other
/// buys nothing and costs a parse. An unstamped candidate cannot be shown to be newer, so it loses to
/// a stamped baked copy — the alternative would let a file with no provenance displace one with some.
/// An unstamped BAKED copy is beaten by any stamp, which is what lets an update reach the catalogs
/// compiled before the field existed.
pub fn is_newer(candidate: Option<&str>, baked: Option<&str>) -> bool {
    match (candidate, baked) {
        (Some(c), Some(b)) => c > b,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Decide what to do with the file at `path`, given the baked catalog's stamp.
///
/// `read` is injected so the decision is testable without a filesystem — every branch here is a
/// refusal, and a refusal that cannot be tested is a refusal nobody has seen happen.
pub fn decide(
    path: &Path,
    baked_generated_at: Option<&str>,
    read: impl FnOnce(&Path) -> std::io::Result<String>,
) -> (Decision, Option<crate::catalog_v2::Catalog>) {
    let shown = path.display().to_string();
    let text = match read(path) {
        Ok(text) => text,
        // Absent is the normal case and must stay silent. Any OTHER io error — a permission
        // problem, a directory where a file belongs — is a fallback worth stating, because it means
        // an update was intended and is not in force.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Decision::Absent, None),
        Err(e) => {
            return (
                Decision::FellBack {
                    path: shown,
                    reason: format!("unreadable: {e}"),
                },
                None,
            );
        }
    };
    let catalog = match crate::catalog_v2::parse(&text) {
        Ok(catalog) => catalog,
        Err(reason) => {
            return (
                Decision::FellBack {
                    path: shown,
                    reason,
                },
                None,
            );
        }
    };
    if !is_newer(catalog.generated_at.as_deref(), baked_generated_at) {
        return (
            Decision::FellBack {
                path: shown,
                reason: match catalog.generated_at.as_deref() {
                    // The stale-copy case this ordering exists for. Naming both stamps is what lets
                    // a reader tell "left over from an older nub" from "clock skew".
                    Some(stamp) => format!(
                        "generated {stamp}, not newer than the compiled catalog ({})",
                        baked_generated_at.unwrap_or("unstamped")
                    ),
                    None => {
                        String::from("no provenance.generatedAt, so it cannot be shown to be newer")
                    }
                },
            },
            None,
        );
    }
    let decision = Decision::Loaded {
        path: shown,
        generated_at: catalog.generated_at.clone().unwrap_or_default(),
        packages: catalog.packages.len(),
    };
    (decision, Some(catalog))
}

/// The data directory, registered once by the binary at startup.
///
/// ⛔ INJECTED, NOT DISCOVERED HERE, AND THE LAYERING IS THE REASON. Resolving it needs nub's
/// precedence rules (`$XDG_DATA_HOME`, `%LOCALAPPDATA%` on Windows, then `~/.local/share`), which live
/// in the CLI beside every other path decision. A second implementation in this crate would be a
/// second answer to "where does nub keep its data", and the two would drift — with the failure mode
/// that the update lands in one place and is read from another, so it silently never applies.
///
/// Unset means the seam is inert, which is the correct default for any embedder that has not opted in.
static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Tell this module where nub keeps its data. Idempotent; the first call wins.
pub fn install(data_dir: PathBuf) {
    let _ = DATA_DIR.set(data_dir);
}

/// The updated catalog, read and validated once, or `None` when none is in force.
///
/// Resolved exactly once per process: this sits on the jail's spawn path, which runs per lifecycle
/// script, and re-reading a megabyte of JSON per script would be a real cost for an answer that cannot
/// change mid-install.
fn resolved() -> &'static (Decision, Option<crate::catalog_v2::Catalog>) {
    static RESOLVED: std::sync::LazyLock<(Decision, Option<crate::catalog_v2::Catalog>)> =
        std::sync::LazyLock::new(|| {
            let Some(path) = catalog_path(DATA_DIR.get().map(PathBuf::as_path)) else {
                return (Decision::Absent, None);
            };
            let baked = crate::catalog_override::baked_v2().and_then(|c| c.generated_at.clone());
            decide(&path, baked.as_deref(), |p| std::fs::read_to_string(p))
        });
    &RESOLVED
}

/// The updated catalog if one is in force, for [`crate::catalog_override::active_v2`].
pub(crate) fn active_v2() -> Option<&'static crate::catalog_v2::Catalog> {
    resolved().1.as_ref()
}

/// What this process decided about an updated catalog, so the caller can print it once.
pub fn decision() -> &'static Decision {
    &resolved().0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal catalog that the real validator accepts, with an optional stamp.
    fn catalog_json(stamp: Option<&str>) -> String {
        let provenance = stamp.map_or(String::from("{}"), |s| {
            format!("{{\"generatedAt\":\"{s}\"}}")
        });
        format!(
            "{{\"packages\":{{\"left-pad\":{{\"default\":{{\"network\":true}}}}}},\
             \"provenance\":{provenance}}}"
        )
    }

    #[test]
    fn a_newer_catalog_replaces_the_compiled_one() {
        let (decision, catalog) =
            decide(Path::new("/d/c.json"), Some("2026-01-01T00:00:00Z"), |_| {
                Ok(catalog_json(Some("2026-08-17T00:00:00Z")))
            });
        assert!(catalog.is_some(), "a newer catalog must be adopted");
        match decision {
            Decision::Loaded {
                generated_at,
                packages,
                ..
            } => {
                assert_eq!(generated_at, "2026-08-17T00:00:00Z");
                assert_eq!(packages, 1);
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    /// ⛔ THE FAILURE THIS ORDERING EXISTS FOR, and it is silent without the check. A file left behind
    /// by an older nub would replace a newer compiled catalog, every package measured since would lose
    /// its entry, and each would drop to the baseline — no error, installs mostly still working, and
    /// the catalog quietly no longer current.
    #[test]
    fn an_older_catalog_is_refused_and_says_so() {
        let (decision, catalog) =
            decide(Path::new("/d/c.json"), Some("2026-08-17T00:00:00Z"), |_| {
                Ok(catalog_json(Some("2026-01-01T00:00:00Z")))
            });
        assert!(catalog.is_none(), "an older catalog must NOT be adopted");
        let banner = decision.banner().expect("a fallback must always be stated");
        assert!(
            banner.contains("not newer"),
            "the reason must name the cause: {banner}"
        );
        assert!(
            banner.contains("2026-01-01T00:00:00Z") && banner.contains("2026-08-17T00:00:00Z"),
            "both stamps must appear, or a reader cannot tell staleness from clock skew: {banner}"
        );
    }

    #[test]
    fn an_equal_stamp_is_refused_because_it_is_the_same_catalog() {
        let same = "2026-08-17T00:00:00Z";
        let (_, catalog) = decide(Path::new("/d/c.json"), Some(same), |_| {
            Ok(catalog_json(Some(same)))
        });
        assert!(
            catalog.is_none(),
            "replacing a catalog with itself buys nothing"
        );
    }

    #[test]
    fn an_unstamped_candidate_loses_to_a_stamped_baked_catalog() {
        let (decision, catalog) =
            decide(Path::new("/d/c.json"), Some("2026-08-17T00:00:00Z"), |_| {
                Ok(catalog_json(None))
            });
        assert!(
            catalog.is_none(),
            "a file with no provenance must not displace one with some"
        );
        assert!(decision.banner().unwrap().contains("generatedAt"));
    }

    #[test]
    fn a_stamped_candidate_reaches_an_unstamped_baked_catalog() {
        // The transitional case that makes this feature usable at all: catalogs baked before the
        // field existed have no stamp, and refusing them would be refusing the floor.
        let (_, catalog) = decide(Path::new("/d/c.json"), None, |_| {
            Ok(catalog_json(Some("2026-08-17T00:00:00Z")))
        });
        assert!(catalog.is_some());
    }

    /// ⛔ ABSENT IS SILENT; EVERY OTHER FAILURE IS LOUD. Almost nobody has this file, so a banner on
    /// its absence would print on every install and train the reader to ignore the line that matters.
    #[test]
    fn a_missing_file_is_silent_but_an_unreadable_one_is_not() {
        let (absent, catalog) = decide(Path::new("/d/c.json"), None, |_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        });
        assert_eq!(absent, Decision::Absent);
        assert_eq!(absent.banner(), None, "the common case must not print");
        assert!(catalog.is_none());

        let (denied, _) = decide(Path::new("/d/c.json"), None, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
        });
        let banner = denied
            .banner()
            .expect("an intended-but-unusable update must be stated");
        assert!(banner.contains("unreadable"), "{banner}");
    }

    /// A malformed catalog falls back rather than aborting, and never to "grant everything".
    #[test]
    fn invalid_json_falls_back_with_the_validator_s_own_reason() {
        let (decision, catalog) = decide(Path::new("/d/c.json"), None, |_| {
            Ok(String::from("{ not json"))
        });
        assert!(catalog.is_none());
        assert!(decision.banner().unwrap().contains("valid JSON"));
    }

    /// The validator is shared with `build.rs`, so a shape the build would reject is rejected here.
    #[test]
    fn a_catalog_the_build_would_reject_is_rejected_here_too() {
        let (_, catalog) = decide(Path::new("/d/c.json"), None, |_| {
            Ok(String::from("{\"nothing\":true}"))
        });
        assert!(
            catalog.is_none(),
            "a catalog with no `packages` table must not load"
        );
    }

    #[test]
    fn the_path_is_fixed_under_the_data_directory_and_takes_no_input() {
        let p = catalog_path(Some(Path::new("/home/u/.local/share/nub"))).expect("a path");
        assert!(
            p.ends_with("catalog/build-jail-catalog-v2.json"),
            "{}",
            p.display()
        );
        assert_eq!(
            catalog_path(None),
            None,
            "no data dir means no update path, not a guess"
        );
    }

    #[test]
    fn ordering_is_lexicographic_and_that_is_sound_for_the_validated_shape() {
        // Same-width zero-padded UTC is why `>` is correct here; the parser rejects anything else.
        assert!(is_newer(
            Some("2026-08-17T00:00:00Z"),
            Some("2026-08-09T23:59:59Z")
        ));
        assert!(is_newer(
            Some("2027-01-01T00:00:00Z"),
            Some("2026-12-31T23:59:59Z")
        ));
        assert!(!is_newer(
            Some("2026-08-09T00:00:00Z"),
            Some("2026-08-17T00:00:00Z")
        ));
    }
}
