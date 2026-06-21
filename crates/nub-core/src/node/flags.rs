//! Version-keyed flag injection with three-stage opt-out merging.
//!
//! The version-banded UNFLAG logic and the webstorage gating predicates here are
//! DERIVED from [`super::feature_matrix`] — the single canonical feature ×
//! Node-version mitigation table. This module no longer carries its own copy of
//! those bands; it iterates the matrix (`unflag_flags_for`) and reads the
//! webstorage feature's bands. Edit the matrix, not a parallel table here. The
//! always-inject flags below (`--enable-source-maps`) and the two version-gated
//! hygiene injections (`--disable-warning`, `--test-coverage-exclude`) are nub's
//! own startup hygiene, not user-facing *features*, so they stay local.

use super::feature_matrix::{self, Mitigation};
use super::version::NodeVersion;

/// Flags Nub injects on EVERY supported Node version where they are safe.
/// `--enable-source-maps` has existed since Node 12.12, so it is structurally
/// available across the whole 18.19+ range — BUT it is gated out of the
/// `source_maps_safe`-false band (Node 26.2.x; see that predicate).
/// (`--disable-warning` is NOT here — it doesn't exist on Node 18.x / 20.0–20.10
/// and is gated below; injecting it there is a hard "bad option" / "not allowed
/// in NODE_OPTIONS" error, which broke the compat tier on those versions.)
const ALWAYS_INJECT: &[&str] = &["--enable-source-maps"];

/// Whether nub may inject `--enable-source-maps` on this Node version.
///
/// Node **26.2.x specifically** has a regression where, with source maps enabled,
/// a no-message `assert.ok(false)` / `assert(false)` rethrows as a `TypeError`
/// instead of the expected `AssertionError` (the source-map remapping path
/// mis-constructs the error for the no-message form). Empirically isolated to the
/// 26.2 patch band: Node 18.19 / 20 / 22 / 24 / 25 and 26.1 are all clean, and a
/// future 26.3 is expected clean too. So nub withholds the injection ONLY on
/// 26.2.x — source maps are unavailable there (a cosmetic loss: stack traces are
/// not remapped), which is far better than corrupting the type of a thrown
/// AssertionError. Verified on real Node 26.2.0:
/// `node --enable-source-maps -e 'try{require("assert").ok(false)}catch(e){console.log(e.constructor.name)}'`
/// prints `TypeError`; without the flag it prints `AssertionError`.
pub fn source_maps_safe(node_version: &NodeVersion) -> bool {
    !(node_version.major() == 26 && node_version.minor() == 2)
}

/// `--disable-warning=ExperimentalWarning` (suppresses Node's experimental-feature
/// warning) was added in Node 21.3.0 and backported to 20.11.0. It does NOT exist
/// on 18.x or 20.0–20.10, where passing it aborts the process ("bad option" as
/// argv, "not allowed in NODE_OPTIONS" via env). Inject it only at/above this
/// floor; below it the (cosmetic) experimental warning is left unsuppressed — far
/// better than refusing to start. Verified against real Node 18.19 / 20.11 / 22.13.
const MIN_DISABLE_WARNING: NodeVersion = NodeVersion::new(20, 11, 0);

/// Whether the target Node has Web Storage at all — DERIVED from the matrix: true
/// iff the `webstorage` feature has ANY mitigation band covering this version
/// (its floor is 22.4.0, where `--experimental-webstorage` / `--localstorage-file`
/// first exist; below that both are "bad option"). True for every Node >= 22.4 —
/// including 25/26 where the global is native (it still needs a `--localstorage-file`
/// to materialize). Verified empirically on Node 26.2.0 and against .repos/node
/// (v27 pre): `--localstorage-file` alone exposes a working, persistent
/// `localStorage`; the flag without the file does not.
///
/// NOTE: this is version-detection logic the spawn path uses indirectly via
/// `webstorage_flag_needed` (which derives from the same matrix). It is the
/// canonical description of Node's Web Storage banding (the source of truth shared
/// with the `webstorage` feature_matrix row).
pub fn webstorage_supported(node_version: &NodeVersion) -> bool {
    feature_matrix::feature("webstorage")
        .mitigation_for(node_version)
        .is_some()
}

/// Whether enabling Web Storage requires the `--experimental-webstorage` FLAG (as
/// opposed to just a `--localstorage-file` path) — DERIVED from the matrix: true
/// iff the `webstorage` feature's mitigation at this version is an `Unflag` band
/// (the 22.4–24.x range, where the feature is still flag-gated). On 25+ the matrix
/// records a `StorageFile` band (the flag defaults on, PR nodejs/node#57666), so
/// only `--localstorage-file` is needed there.
///
/// The spawn path (`spawn.rs`) consumes this to decide where to ALWAYS inject
/// `--experimental-webstorage` (the maintainer, 2026-06-15): on the flag-needed band the
/// flag is unconditionally injected so `sessionStorage` works out of the box; below
/// the band it's a "bad option" and on 25+ it's unnecessary (native).
pub fn webstorage_flag_needed(node_version: &NodeVersion) -> bool {
    matches!(
        feature_matrix::feature("webstorage").mitigation_for(node_version),
        Some(Mitigation::Unflag(_))
    )
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

/// Compute the flags Nub should inject for the given Node version,
/// after subtracting any user opt-outs from argv and NODE_OPTIONS.
///
/// `show_warnings`: if true, suppress the `--disable-warning=ExperimentalWarning`
/// injection (Nub's `--show-warnings` flag).
///
/// ## Verified conflict semantics (probed on real Node 18.19 / 22.15 / 25.8 / 26.2)
///
/// nub injects its positive flags into BOTH the child argv (in `spawn.rs`, AHEAD
/// of the user's argv) and the inherited NODE_OPTIONS. Node's resolution is
/// **argv last-wins, and argv beats NODE_OPTIONS**. That asymmetry is exactly why
/// a plain "is it a crash?" check is not enough — a user's *disable* can be
/// silently OVERRIDDEN even when nothing crashes. The Stage-2/3 subtraction below
/// is the uniform fix: every user negation (positive or `--no-…`, argv or env) is
/// removed from nub's inject set, so nub never emits a positive that competes with
/// a user disable. Probe results, per scenario:
///
/// | flag class                        | scenario                                  | raw Node behavior            | nub mechanism                |
/// |-----------------------------------|-------------------------------------------|------------------------------|------------------------------|
/// | boolean experimental (`vm`, …)    | dup positive (argv×2; argv+NODE_OPTIONS)  | exit 0, ENABLED (idempotent) | safe-duplicate, no action    |
/// | boolean experimental              | `--no-x` argv, nub `+x` argv (after user) | exit 0, disabled (argv last) | also subtracted → not emitted|
/// | boolean experimental              | `--no-x` in NODE_OPTIONS, nub `+x` in argv| exit 0, **ENABLED** (argv>env: nub OVERRIDES user disable) | **subtracted**: `collect_negations` scans NODE_OPTIONS → `+x` dropped |
/// | `--enable-source-maps`            | `--no-enable-source-maps` (any channel)   | exit 0; disables when it wins| **subtracted** via `--no-enable-` prefix |
/// | value-bearing (`--disable-warning`,| user `=<other value>` alongside nub's     | exit 0; repeatable, additive | safe-duplicate: nub adds its own value, never stomps the user's (Node accepts multiple) |
/// | `--test-coverage-exclude`)        |                                           |                              |                              |
/// | below-floor flags (18.19)         | `--experimental-webstorage`, `--disable-warning`, `--test-coverage-exclude` | **exit 9 "bad option"** | band-gated OUT (never injected there) |
///
/// (Webstorage's `--experimental-webstorage` is injected in `spawn.rs`, not here —
/// always-injected on the `webstorage_flag_needed` band, suppressed only when the
/// user already supplied the flag in either polarity; see there.)
pub fn compute_inject_flags(
    node_version: NodeVersion,
    user_argv: &[String],
    node_options: Option<&str>,
    show_warnings: bool,
) -> Vec<&'static str> {
    // Stage 1: compute the would-inject set.
    let mut flags: Vec<&str> = Vec::new();

    for &flag in ALWAYS_INJECT {
        // --enable-source-maps is withheld on Node 26.2.x (see `source_maps_safe`):
        // there it turns a no-message AssertionError into a TypeError.
        if flag == "--enable-source-maps" && !source_maps_safe(&node_version) {
            continue;
        }
        flags.push(flag);
    }

    // Warning suppression — only where the flag exists (>= 20.11) and the user
    // hasn't asked to see warnings.
    if !show_warnings && node_version >= MIN_DISABLE_WARNING {
        flags.push("--disable-warning=ExperimentalWarning");
    }

    // The version-banded experimental unflags are DERIVED from the canonical
    // feature matrix — for each feature whose mitigation at this version is
    // `Unflag(flag)`, the flag is injected. Tuned per band so the flag is present
    // exactly where it both EXISTS (else "bad option" / "not allowed in
    // NODE_OPTIONS" startup abort) and is still REQUIRED (not yet default-on). See
    // `feature_matrix::FEATURES` for the bands + changelog evidence. (webstorage's
    // flag is injected separately in spawn.rs, since it pairs with a
    // runtime-computed `--localstorage-file` path — but its bands live in the same
    // matrix, read via `webstorage_flag_needed` / `webstorage_supported`.)
    for flag in feature_matrix::unflag_flags_for(&node_version) {
        // Skip the webstorage flag here: spawn.rs owns its injection (paired with
        // the workspace-keyed --localstorage-file), gated on the same matrix bands
        // via `webstorage_flag_needed`. Injecting it in this static set too would
        // emit it without the file (the global never materializes) and bypass the
        // user-override suppression spawn.rs applies.
        if flag == "--experimental-webstorage" {
            continue;
        }
        flags.push(flag);
    }

    // Stage 2: parse user opt-outs from argv and NODE_OPTIONS.
    let user_negations = collect_negations(user_argv, node_options);

    // Stage 3: subtract.
    flags.retain(|flag| !user_negations.iter().any(|neg| is_negation_of(neg, flag)));

    flags
}

/// The minimum Node version at which `flag` EXISTS (is accepted, not "bad option"
/// / "not allowed in NODE_OPTIONS") — `None` if `flag` is not a version-gated flag
/// nub knows about.
///
/// This is the SINGLE SOURCE OF TRUTH for gated-flag floors, shared by the inject
/// path (which adds a gated flag only at/above its floor) and the strip path
/// (`strip_unsupported_node_options`, which removes a gated flag from an inherited
/// NODE_OPTIONS when the child Node sits below its floor). It is DERIVED, not a
/// hand-maintained parallel list:
///   * the experimental `Unflag(flag)` families come straight from the feature
///     matrix — a flag's existence floor is the LOWEST `lo` of any band that
///     unflags it. Once a flag exists Node keeps ACCEPTING it (as a default-true
///     no-op bool) at every higher version, so the "rejected" band is exactly
///     `[0, floor)` and a single floor per flag is sufficient.
///   * `--disable-warning` and `--test-coverage-exclude` are nub's own hygiene
///     injections (not user-facing *features*), so their floors live as consts in
///     this module; they are reused here rather than duplicated.
fn flag_existence_floor(flag: &str) -> Option<NodeVersion> {
    // nub's own hygiene flags (consts above), not in the feature matrix.
    if flag == "--disable-warning" {
        return Some(MIN_DISABLE_WARNING);
    }
    if flag == "--test-coverage-exclude" {
        return Some(MIN_TEST_COVERAGE_EXCLUDE);
    }
    // Experimental unflag families: the floor is the lowest `lo` across every
    // band that unflags this exact flag. Derived from the matrix — no parallel
    // table. (Webstorage's `--experimental-webstorage` is in the matrix too; its
    // floor of 22.4.0 is recovered here just like the rest.)
    feature_matrix::unflag_floor(flag)
}

/// Remove from an inherited `NODE_OPTIONS` string any version-gated flag whose
/// existence floor EXCEEDS the child's Node version, preserving every other token
/// in order. Returns the rewritten string (possibly empty).
///
/// ## Why
/// nub propagates flags to child Node processes via `NODE_OPTIONS`. Flags nub ITSELF
/// adds this hop are already version-floor-gated. But an INHERITED `NODE_OPTIONS`
/// (set by an ancestor nub, or genuinely by the user) is otherwise appended
/// verbatim — so a gated flag in it (`--experimental-webstorage`, `--disable-warning`,
/// `--test-coverage-exclude`, `--experimental-sqlite`, …) reaches a child Node too
/// old to parse it and aborts with exit 9 ("not allowed in NODE_OPTIONS"). Snipping
/// the below-floor flag is strictly better than exit-9, regardless of who set it.
///
/// ## Token forms handled
/// * bare valueless flag — `--experimental-webstorage` (the whole `--experimental-*`
///   family is boolean): the single token is dropped.
/// * `--flag=value` — `--disable-warning=ExperimentalWarning`,
///   `--test-coverage-exclude=glob`: matched by the exact flag name followed by `=`,
///   then the whole token is dropped.
///
/// ## Documented v1 gap (simple path + a loud note, per repo style)
/// The rare SPACE-separated `--flag value` form (e.g. `--disable-warning Foo` as two
/// tokens) is OUT OF SCOPE: we do not detect/drop a following value token. `=` is the
/// conventional NODE_OPTIONS spelling for value-bearing flags, so this gap is uncommon.
/// If a space-separated below-floor value flag ever appears in an inherited
/// NODE_OPTIONS, its value token survives and Node may complain about the stray value
/// — but the gated flag itself (the exit-9 trigger) is still removed.
pub fn strip_unsupported_node_options(node_options: &str, node_version: &NodeVersion) -> String {
    node_options
        .split_whitespace()
        .filter(|token| {
            // The flag name is the token up to the first `=` (value form) or the
            // whole token (valueless form). Match the EXACT name against the floor
            // table; never a prefix, so an unrelated longer token isn't mangled.
            let name = token.split('=').next().unwrap_or(token);
            match flag_existence_floor(name) {
                // Gated flag below the child's floor → snip it.
                Some(floor) => *node_version >= floor,
                // Not a gated flag nub knows about → always preserve.
                None => true,
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
        // webstorage is NOT in this static set: spawn.rs owns its injection (it
        // always injects --experimental-webstorage on the 22.4–24 flag-needed band,
        // paired with no synthesized --localstorage-file). See spawn.rs.
        assert!(!flags.contains(&"--experimental-webstorage"));
        // sqlite is unflagged on 22.13.0+ (the 22.x line), so 22.15 does NOT inject it.
        assert!(!flags.contains(&"--experimental-sqlite"));
    }

    #[test]
    fn vm_modules_injected_across_entire_floor() {
        // vm.Module is never unflagged — inject from the 18.19 floor through 26.x.
        // (Regression: the old min:22.15.0 left vm.Module broken on 18.19–22.14.)
        assert!(
            compute_inject_flags(v(18, 19, 0), &[], None, false)
                .contains(&"--experimental-vm-modules")
        );
        assert!(
            compute_inject_flags(v(26, 2, 0), &[], None, false)
                .contains(&"--experimental-vm-modules")
        );
    }

    #[test]
    fn eventsource_skips_the_21x_hole() {
        // EventSource landed at 22.3.0 + 20.18.0 backport; never shipped on 21.x.
        // Injecting on 21.x is a "bad option" crash — the highest-stakes boundary here.
        let yes = "--experimental-eventsource";
        assert!(!compute_inject_flags(v(20, 17, 0), &[], None, false).contains(&yes));
        assert!(compute_inject_flags(v(20, 18, 0), &[], None, false).contains(&yes));
        // The hole: must NOT inject anywhere on the 21.x line.
        assert!(
            !compute_inject_flags(v(21, 0, 0), &[], None, false).contains(&yes),
            "must NOT inject --experimental-eventsource on 21.0 (flag never existed there → crash)"
        );
        assert!(!compute_inject_flags(v(22, 2, 0), &[], None, false).contains(&yes));
        assert!(compute_inject_flags(v(22, 3, 0), &[], None, false).contains(&yes));
        assert!(compute_inject_flags(v(26, 2, 0), &[], None, false).contains(&yes));
    }

    #[test]
    fn sqlite_injected_only_in_the_two_flagged_bands() {
        // node:sqlite: flag added 22.5.0, unflagged 22.13.0 (22.x) and 23.4.0 (23.x).
        // Inject only where the flag exists AND is still required.
        let sql = "--experimental-sqlite";
        assert!(!compute_inject_flags(v(22, 4, 0), &[], None, false).contains(&sql)); // flag absent
        assert!(compute_inject_flags(v(22, 5, 0), &[], None, false).contains(&sql)); // band 1 floor
        assert!(compute_inject_flags(v(22, 12, 0), &[], None, false).contains(&sql));
        assert!(!compute_inject_flags(v(22, 13, 0), &[], None, false).contains(&sql)); // unflagged on 22.x
        assert!(compute_inject_flags(v(23, 3, 0), &[], None, false).contains(&sql)); // band 2
        assert!(!compute_inject_flags(v(23, 4, 0), &[], None, false).contains(&sql)); // unflagged on 23.x
        assert!(!compute_inject_flags(v(24, 0, 0), &[], None, false).contains(&sql)); // unflagged everywhere after
    }

    #[test]
    fn websocket_injected_only_on_flag_gated_band() {
        // WebSocket global is flag-gated on [20.10.0, 22.0.0): exists on 20.10+ and all
        // 21.x, default-on from 22.0.0. Below 20.10 the flag doesn't exist ("bad option").
        let ws = "--experimental-websocket";
        assert!(!compute_inject_flags(v(20, 9, 0), &[], None, false).contains(&ws));
        assert!(compute_inject_flags(v(20, 10, 0), &[], None, false).contains(&ws));
        assert!(compute_inject_flags(v(21, 5, 0), &[], None, false).contains(&ws)); // all of 21.x
        assert!(!compute_inject_flags(v(22, 0, 0), &[], None, false).contains(&ws)); // default-on
    }

    #[test]
    fn wasm_modules_injected_only_in_the_two_flagged_bands() {
        // Wasm ES-module imports: flag exists since Node 12 (below the floor),
        // default-on at 24.5.0 (24.x) and 22.19.0 (22.x) via #57038; never
        // default-on on the 23.x line (EOL before the backport). Inject on
        // [18.19, 22.19) ∪ [23.0, 24.5).
        let w = "--experimental-wasm-modules";
        assert!(compute_inject_flags(v(18, 19, 0), &[], None, false).contains(&w)); // floor
        assert!(compute_inject_flags(v(22, 13, 0), &[], None, false).contains(&w));
        assert!(compute_inject_flags(v(22, 18, 0), &[], None, false).contains(&w));
        assert!(!compute_inject_flags(v(22, 19, 0), &[], None, false).contains(&w)); // default-on 22.x
        assert!(compute_inject_flags(v(23, 2, 0), &[], None, false).contains(&w)); // 23.x stays flagged
        assert!(compute_inject_flags(v(24, 4, 0), &[], None, false).contains(&w));
        assert!(!compute_inject_flags(v(24, 5, 0), &[], None, false).contains(&w)); // default-on 24.x
        assert!(!compute_inject_flags(v(26, 0, 0), &[], None, false).contains(&w));
    }

    #[test]
    fn shadow_realm_injected_across_the_whole_floor() {
        // ShadowRealm: flag exists from 18.13/19.0 (below the floor), never made
        // default-on through Node 26 (TC39 Stage 3). Inject from 18.19 to infinity.
        let s = "--experimental-shadow-realm";
        assert!(compute_inject_flags(v(18, 19, 0), &[], None, false).contains(&s));
        assert!(compute_inject_flags(v(22, 19, 0), &[], None, false).contains(&s));
        assert!(compute_inject_flags(v(26, 0, 0), &[], None, false).contains(&s));
    }

    #[test]
    fn user_opt_out_via_argv() {
        let argv = vec!["--no-experimental-vm-modules".to_string()];
        let flags = compute_inject_flags(v(22, 15, 0), &argv, None, false);
        assert!(!flags.contains(&"--experimental-vm-modules"));
        // Other flags still present (eventsource is in-band at 22.15).
        assert!(flags.contains(&"--experimental-eventsource"));
    }

    #[test]
    fn user_opt_out_via_node_options() {
        // Use 22.12.0 where sqlite IS injected (first band), so the opt-out is observable.
        let flags = compute_inject_flags(
            v(22, 12, 0),
            &[],
            Some("--no-experimental-sqlite --max-old-space-size=4096"),
            false,
        );
        assert!(!flags.contains(&"--experimental-sqlite"));
        assert!(flags.contains(&"--experimental-vm-modules"));
    }

    #[test]
    fn no_enable_source_maps_wins_over_always_inject() {
        // `--enable-source-maps` is in ALWAYS_INJECT, but a user's explicit
        // `--no-enable-source-maps` must clobber it — nub never re-enables over a
        // user disable (the maintainer, 2026-06-11). Verified on real Node 22.15: the
        // `--no-` form is accepted (exit 0) and disables source maps when it wins;
        // since nub injects the positive into argv AHEAD of the user's, an unsub-
        // tracted positive would re-enable it. Subtraction is the fix, in BOTH
        // channels (argv and NODE_OPTIONS).
        let argv = vec!["--no-enable-source-maps".to_string()];
        assert!(
            !compute_inject_flags(v(22, 15, 0), &argv, None, false)
                .contains(&"--enable-source-maps"),
            "user --no-enable-source-maps (argv) must suppress nub's always-inject"
        );
        assert!(
            !compute_inject_flags(v(22, 15, 0), &[], Some("--no-enable-source-maps"), false)
                .contains(&"--enable-source-maps"),
            "user --no-enable-source-maps (NODE_OPTIONS) must suppress it too"
        );
    }

    #[test]
    fn user_disable_warning_with_a_different_value_is_not_stomped() {
        // `--disable-warning` is value-bearing and REPEATABLE in Node (verified:
        // two `--disable-warning=<diff>` coexist, exit 0). nub injects its own
        // `=ExperimentalWarning` ADDITIVELY — it must not drop or replace a user's
        // `--disable-warning=DeprecationWarning`. The subtraction path only removes
        // `--no-…` negations, so a positive value-bearing user flag passes through
        // untouched (it rides in the user's own argv/NODE_OPTIONS) while nub's
        // value is still injected.
        let flags = compute_inject_flags(
            v(22, 15, 0),
            &["--disable-warning=DeprecationWarning".to_string()],
            None,
            false,
        );
        assert!(
            flags.contains(&"--disable-warning=ExperimentalWarning"),
            "nub injects its own warning suppression alongside the user's different value"
        );
    }

    #[test]
    fn show_warnings_suppresses_warning_flag() {
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, true);
        assert!(!flags.contains(&"--disable-warning=ExperimentalWarning"));
        assert!(flags.contains(&"--enable-source-maps"));
    }

    #[test]
    fn floor_injects_only_universally_safe_flags() {
        // At 20.0.0: --enable-source-maps and vm-modules (whole-floor) inject, but the
        // version-gated entries do not — sqlite/eventsource/websocket flags don't exist
        // here ("bad option"), and --disable-warning is below its 20.11 floor.
        let flags = compute_inject_flags(v(20, 0, 0), &[], None, false);
        assert!(flags.contains(&"--enable-source-maps"));
        assert!(flags.contains(&"--experimental-vm-modules"));
        assert!(!flags.contains(&"--experimental-sqlite"));
        assert!(!flags.contains(&"--experimental-eventsource"));
        assert!(!flags.contains(&"--experimental-websocket")); // below 20.10 floor
        assert!(!flags.contains(&"--disable-warning=ExperimentalWarning"));
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
    fn webstorage_supported_floor_is_22_4() {
        // Below 22.4 the webstorage flags don't exist ("bad option") — so the
        // --localstorage-file injection (and webstorage entirely) is skipped. At/above
        // 22.4 it's supported on EVERY version, including the native 25/26 (the file is
        // still required there for the global to materialize).
        assert!(!webstorage_supported(&v(18, 19, 0)));
        assert!(!webstorage_supported(&v(20, 11, 0)));
        assert!(!webstorage_supported(&v(22, 3, 0)));
        assert!(webstorage_supported(&v(22, 4, 0)));
        assert!(webstorage_supported(&v(22, 13, 0)));
        assert!(webstorage_supported(&v(24, 0, 0)));
        assert!(webstorage_supported(&v(25, 0, 0)));
        assert!(webstorage_supported(&v(26, 2, 0)));
    }

    #[test]
    fn experimental_webstorage_flag_only_needed_on_22_4_through_24() {
        // The --experimental-webstorage FLAG is only needed where the feature is
        // flag-gated. It was unflagged (defaults on) in Node 25.0.0, so on 25+ nub
        // injects only --localstorage-file, not the experimental flag.
        assert!(!webstorage_flag_needed(&v(22, 3, 0))); // flag doesn't exist yet
        assert!(webstorage_flag_needed(&v(22, 4, 0))); // floor: flag needed
        assert!(webstorage_flag_needed(&v(22, 15, 0)));
        assert!(webstorage_flag_needed(&v(24, 0, 0)));
        assert!(webstorage_flag_needed(&v(24, 99, 0))); // still flagged through 24.x
        assert!(!webstorage_flag_needed(&v(25, 0, 0))); // native — flag not needed
        assert!(!webstorage_flag_needed(&v(26, 2, 0)));
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

    #[test]
    fn strips_gated_node_options_flag_below_floor_both_token_forms() {
        // Node 20.0 is below --experimental-webstorage (22.4), --disable-warning
        // (20.11), and --test-coverage-exclude (22.5). Both the valueless and the
        // `=value` forms must be removed, or the child aborts with exit 9.
        let opts = "--experimental-webstorage --disable-warning=ExperimentalWarning --test-coverage-exclude=foo/**";
        assert_eq!(strip_unsupported_node_options(opts, &v(20, 0, 0)), "");
    }

    #[test]
    fn preserves_gated_node_options_flag_at_or_above_floor() {
        // On 22.15 all three are at/above floor → kept verbatim, in order.
        let opts = "--experimental-webstorage --disable-warning=ExperimentalWarning --test-coverage-exclude=foo/**";
        assert_eq!(strip_unsupported_node_options(opts, &v(22, 15, 0)), opts);
    }

    #[test]
    fn preserves_non_gated_node_options_tokens() {
        // --max-old-space-size is not version-gated by nub; it must survive on any
        // version, and an unrelated longer token must not be prefix-mangled by the
        // --experimental-webstorage match.
        let opts = "--max-old-space-size=4096 --experimental-webstorage-extra=1";
        assert_eq!(strip_unsupported_node_options(opts, &v(18, 19, 0)), opts);
    }

    #[test]
    fn strips_only_the_below_floor_token_keeping_neighbors() {
        // On 20.11: --disable-warning is now at floor (kept) but
        // --experimental-webstorage (22.4) and --test-coverage-exclude (22.5) are
        // still below floor (dropped). Surviving tokens keep their order.
        let opts = "--experimental-webstorage --disable-warning=ExperimentalWarning --max-old-space-size=2048 --test-coverage-exclude=x";
        assert_eq!(
            strip_unsupported_node_options(opts, &v(20, 11, 0)),
            "--disable-warning=ExperimentalWarning --max-old-space-size=2048"
        );
    }

    #[test]
    fn strip_is_a_no_op_on_empty_node_options() {
        assert_eq!(strip_unsupported_node_options("", &v(18, 19, 0)), "");
    }

    #[test]
    fn watch_file_path_strips_below_floor_node_options_before_child_spawn() {
        // The `nub watch <file>` path spawns `node` directly and explicitly sets the
        // stripped NODE_OPTIONS on the child (cli.rs run_watch_file), routing the
        // inherited value through this fn against the watch invocation's resolved
        // Node version. On a pin below 22.4, an inherited --experimental-webstorage
        // must be snipped or the watch child aborts with exit 9; the non-gated
        // --max-old-space-size neighbor must survive.
        let inherited = "--experimental-webstorage --max-old-space-size=4096";
        assert_eq!(
            strip_unsupported_node_options(inherited, &v(20, 19, 0)),
            "--max-old-space-size=4096"
        );
    }

    #[test]
    fn source_maps_withheld_only_on_26_2_band() {
        // Node 26.2.x regresses: with --enable-source-maps, a no-message
        // assert.ok(false) rethrows as TypeError instead of AssertionError. nub
        // withholds the injection there and ONLY there — 24 / 25 / 26.1 and a
        // future 26.3 are clean. (Verified empirically on real Node 26.2.0.)
        for ver in [v(24, 0, 0), v(25, 8, 0), v(26, 1, 0), v(26, 3, 0)] {
            assert!(
                source_maps_safe(&ver),
                "source maps must be safe to inject on {ver:?}"
            );
            assert!(
                compute_inject_flags(ver.clone(), &[], None, false)
                    .contains(&"--enable-source-maps"),
                "--enable-source-maps must inject on {ver:?}"
            );
        }
        // The affected band: every 26.2.x patch is gated out.
        for ver in [v(26, 2, 0), v(26, 2, 5)] {
            assert!(
                !source_maps_safe(&ver),
                "source maps must be withheld on {ver:?}"
            );
            assert!(
                !compute_inject_flags(ver.clone(), &[], None, false)
                    .contains(&"--enable-source-maps"),
                "--enable-source-maps must NOT inject on {ver:?} (assert→TypeError regression)"
            );
        }
    }
}
