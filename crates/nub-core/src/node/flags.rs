//! Version-keyed flag injection with three-stage opt-out merging.

use super::version::NodeVersion;

/// Flags Nub injects on EVERY supported Node version. `--enable-source-maps` has
/// existed since Node 12.12, so it is safe across the whole 18.19+ range.
/// (`--disable-warning` is NOT here — it doesn't exist on Node 18.x / 20.0–20.10
/// and is gated below; injecting it there is a hard "bad option" / "not allowed
/// in NODE_OPTIONS" error, which broke the compat tier on those versions.)
const ALWAYS_INJECT: &[&str] = &["--enable-source-maps"];

/// `--disable-warning=ExperimentalWarning` (suppresses Node's experimental-feature
/// warning) was added in Node 21.3.0 and backported to 20.11.0. It does NOT exist
/// on 18.x or 20.0–20.10, where passing it aborts the process ("bad option" as
/// argv, "not allowed in NODE_OPTIONS" via env). Inject it only at/above this
/// floor; below it the (cosmetic) experimental warning is left unsuppressed — far
/// better than refusing to start. Verified against real Node 18.19 / 20.11 / 22.13.
const MIN_DISABLE_WARNING: NodeVersion = NodeVersion::new(20, 11, 0);

/// `--experimental-webstorage` + `--localstorage-file` landed in Node 22.4.0.
/// Below it both are "bad option" — so nub's default-on webstorage must be gated
/// on this floor (the compat tier 18.19–22.3 simply runs without webstorage).
pub const MIN_WEBSTORAGE: NodeVersion = NodeVersion::new(22, 4, 0);

/// Whether the target Node supports `--experimental-webstorage`. Callers gate the
/// webstorage flag injection on this so older compat-tier Node isn't handed a flag
/// it rejects.
pub fn webstorage_supported(node_version: &NodeVersion) -> bool {
    *node_version >= MIN_WEBSTORAGE
}

/// `--test-coverage-exclude=<glob>` landed in Node 22.5.0. Below it the flag does
/// not exist: as argv it's a "bad option", and in NODE_OPTIONS it's "not allowed in
/// NODE_OPTIONS" — either way a hard startup abort. nub injects it to keep its own
/// preloaded runtime/*.mjs out of a user's `--experimental-test-coverage` report,
/// but that exclude MUST be gated on this floor — otherwise every nub invocation on
/// 18.19–22.4 dies before running a line (the NODE_OPTIONS form is unconditional).
/// On the compat tier below 22.5 the exclude is simply skipped: nub's runtime shows
/// up in the (rare) coverage report — a cosmetic aggregate quirk, vastly better than
/// refusing to start. Verified against real Node 18.19 / 20.11 / 22.15.
pub const MIN_TEST_COVERAGE_EXCLUDE: NodeVersion = NodeVersion::new(22, 5, 0);

/// Whether the target Node supports `--test-coverage-exclude` (argv or NODE_OPTIONS).
pub fn test_coverage_exclude_supported(node_version: &NodeVersion) -> bool {
    *node_version >= MIN_TEST_COVERAGE_EXCLUDE
}

/// Experimental flags to unflag per the v0.1 set.
/// Each entry: (flag, min Node version where the flag exists, max
/// version where it's still behind the flag — None means "still
/// experimental on all supported versions").
const UNFLAG_TABLE: &[UnflagEntry] = &[
    UnflagEntry {
        flag: "--experimental-vm-modules",
        min: NodeVersion::new(22, 15, 0),
        unflagged_at: None,
    },
    UnflagEntry {
        flag: "--experimental-eventsource",
        min: NodeVersion::new(22, 15, 0),
        unflagged_at: None,
    },
    // webstorage requires --localstorage-file=<path> to be set alongside it.
    // Injecting the flag without the path causes ERR_INVALID_ARG_VALUE.
    // Defer to when we have a default storage path per wiki/runtime/webstorage-unflag.md.
    // UnflagEntry {
    //     flag: "--experimental-webstorage",
    //     min: NodeVersion::new(22, 15, 0),
    //     unflagged_at: None,
    // },
    UnflagEntry {
        flag: "--experimental-sqlite",
        min: NodeVersion::new(22, 15, 0),
        unflagged_at: None,
    },
];

struct UnflagEntry {
    flag: &'static str,
    min: NodeVersion,
    /// If Some, the flag was unflagged at this version and injection
    /// is unnecessary from this version onward.
    unflagged_at: Option<NodeVersion>,
}

/// Compute the flags Nub should inject for the given Node version,
/// after subtracting any user opt-outs from argv and NODE_OPTIONS.
///
/// `show_warnings`: if true, suppress the `--disable-warning=ExperimentalWarning`
/// injection (Nub's `--show-warnings` flag).
pub fn compute_inject_flags(
    node_version: NodeVersion,
    user_argv: &[String],
    node_options: Option<&str>,
    show_warnings: bool,
) -> Vec<&'static str> {
    // Stage 1: compute the would-inject set.
    let mut flags: Vec<&str> = Vec::new();

    for &flag in ALWAYS_INJECT {
        flags.push(flag);
    }

    // Warning suppression — only where the flag exists (>= 20.11) and the user
    // hasn't asked to see warnings.
    if !show_warnings && node_version >= MIN_DISABLE_WARNING {
        flags.push("--disable-warning=ExperimentalWarning");
    }

    for entry in UNFLAG_TABLE {
        if node_version >= entry.min {
            if let Some(ref unflagged_at) = entry.unflagged_at {
                if node_version >= *unflagged_at {
                    continue;
                }
            }
            flags.push(entry.flag);
        }
    }

    // Stage 2: parse user opt-outs from argv and NODE_OPTIONS.
    let user_negations = collect_negations(user_argv, node_options);

    // Stage 3: subtract.
    flags.retain(|flag| !user_negations.iter().any(|neg| is_negation_of(neg, flag)));

    flags
}

/// Collect all `--no-experimental-*` and other negation flags from
/// the user's argv and NODE_OPTIONS.
fn collect_negations(user_argv: &[String], node_options: Option<&str>) -> Vec<String> {
    let mut negations = Vec::new();

    for arg in user_argv {
        if arg.starts_with("--no-experimental-") || arg.starts_with("--no-enable-") {
            negations.push(arg.clone());
        }
    }

    if let Some(opts) = node_options {
        for token in opts.split_whitespace() {
            if token.starts_with("--no-experimental-") || token.starts_with("--no-enable-") {
                negations.push(token.to_string());
            }
        }
    }

    negations
}

/// Returns true if `negation` negates `flag`.
/// e.g., "--no-experimental-vm-modules" negates "--experimental-vm-modules".
fn is_negation_of(negation: &str, flag: &str) -> bool {
    if let Some(rest) = negation.strip_prefix("--no-") {
        let positive = format!("--{rest}");
        positive == flag
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> NodeVersion {
        NodeVersion::new(major, minor, patch)
    }

    #[test]
    fn always_injects_warning_suppression_and_source_maps() {
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, false);
        assert!(flags.contains(&"--disable-warning=ExperimentalWarning"));
        assert!(flags.contains(&"--enable-source-maps"));
    }

    #[test]
    fn injects_unflag_set_on_22_15() {
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, false);
        assert!(flags.contains(&"--experimental-vm-modules"));
        assert!(flags.contains(&"--experimental-eventsource"));
        // webstorage deferred — requires --localstorage-file alongside it
        assert!(flags.contains(&"--experimental-sqlite"));
    }

    #[test]
    fn user_opt_out_via_argv() {
        let argv = vec!["--no-experimental-vm-modules".to_string()];
        let flags = compute_inject_flags(v(22, 15, 0), &argv, None, false);
        assert!(!flags.contains(&"--experimental-vm-modules"));
        // Other flags still present.
        assert!(flags.contains(&"--experimental-sqlite"));
    }

    #[test]
    fn user_opt_out_via_node_options() {
        let flags = compute_inject_flags(
            v(22, 15, 0),
            &[],
            Some("--no-experimental-sqlite --max-old-space-size=4096"),
            false,
        );
        assert!(!flags.contains(&"--experimental-sqlite"));
        assert!(flags.contains(&"--experimental-vm-modules"));
    }

    #[test]
    fn show_warnings_suppresses_warning_flag() {
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, true);
        assert!(!flags.contains(&"--disable-warning=ExperimentalWarning"));
        assert!(flags.contains(&"--enable-source-maps"));
    }

    #[test]
    fn no_flags_below_minimum() {
        // Below 22.15 there should be no unflag entries (they have min 22.15).
        // But ALWAYS_INJECT still applies if the version check were bypassed.
        // In practice, we reject too-old versions in discovery, so this
        // tests the table logic in isolation.
        let flags = compute_inject_flags(v(20, 0, 0), &[], None, false);
        assert!(flags.contains(&"--enable-source-maps"));
        assert!(!flags.contains(&"--experimental-vm-modules"));
    }

    #[test]
    fn disable_warning_gated_to_node_that_supports_it() {
        // Node 18.19 and 20.0–20.10 reject `--disable-warning` ("bad option" /
        // "not allowed in NODE_OPTIONS"), which crashed the compat tier. It must
        // not be injected below 20.11; from 20.11 onward it is.
        for ver in [v(18, 19, 0), v(20, 0, 0), v(20, 10, 0)] {
            let flags = compute_inject_flags(ver.clone(), &[], None, false);
            assert!(
                !flags.contains(&"--disable-warning=ExperimentalWarning"),
                "must NOT inject --disable-warning on {ver:?} (the flag aborts those versions)"
            );
            // --enable-source-maps is always safe, so the floor still augments.
            assert!(
                flags.contains(&"--enable-source-maps"),
                "source-maps must still inject on {ver:?}"
            );
        }
        for ver in [v(20, 11, 0), v(22, 13, 0)] {
            let flags = compute_inject_flags(ver.clone(), &[], None, false);
            assert!(
                flags.contains(&"--disable-warning=ExperimentalWarning"),
                "must inject --disable-warning on {ver:?} (supported there)"
            );
        }
    }

    #[test]
    fn webstorage_gated_to_22_4() {
        // --experimental-webstorage is "bad option" below 22.4 — callers must gate.
        assert!(!webstorage_supported(&v(18, 19, 0)));
        assert!(!webstorage_supported(&v(20, 11, 0)));
        assert!(!webstorage_supported(&v(22, 3, 0)));
        assert!(webstorage_supported(&v(22, 4, 0)));
        assert!(webstorage_supported(&v(22, 13, 0)));
        assert!(webstorage_supported(&v(24, 0, 0)));
    }

    #[test]
    fn test_coverage_exclude_gated_to_22_5() {
        // `--test-coverage-exclude` was added in Node 22.5.0. Below it the flag is
        // rejected in NODE_OPTIONS ("not allowed in NODE_OPTIONS") — and because nub
        // injects it UNCONDITIONALLY whenever a preload is present, an ungated inject
        // aborts EVERY nub invocation on the entire 18.19–22.4 range (the most common
        // LTS lines). Callers must gate on this; this guards the regression.
        assert!(!test_coverage_exclude_supported(&v(18, 19, 0)));
        assert!(!test_coverage_exclude_supported(&v(20, 11, 0)));
        assert!(!test_coverage_exclude_supported(&v(22, 4, 0)));
        assert!(test_coverage_exclude_supported(&v(22, 5, 0)));
        assert!(test_coverage_exclude_supported(&v(22, 15, 0)));
        assert!(test_coverage_exclude_supported(&v(24, 0, 0)));
    }
}
