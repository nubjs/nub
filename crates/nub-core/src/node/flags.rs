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
/// `source_maps_safe`-false band (Node 26.0.0–26.7.x; see that predicate).
/// (`--disable-warning` is NOT here — it doesn't exist on Node 18.x / 20.0–20.10
/// and is gated below; injecting it there is a hard "bad option" / "not allowed
/// in NODE_OPTIONS" error, which broke the compat tier on those versions.)
const ALWAYS_INJECT: &[&str] = &["--enable-source-maps"];

/// Whether nub may inject `--enable-source-maps` on this Node version.
///
/// The whole **Node 26.x** line released so far regresses (nodejs/node#63169):
/// with source maps enabled and no source map for the file, the remapping path's
/// `getErrorSourceLocation` returns `undefined`, so a no-message
/// `assert(false)` / `assert.ok(false)` / `assert.strict(false)` throws
/// `TypeError [ERR_INVALID_ARG_TYPE]` ("The \"message\" argument …") instead of
/// the expected `AssertionError`. Verified on real 26.0.0 / 26.3.0 / 26.5.1 /
/// 26.7.0; 25.x is clean (25.9.0 yields an `AssertionError` with a degraded
/// message, which is the right TYPE). This band was previously — and wrongly —
/// documented as 26.2-only.
///
/// Fixed on Node `main` by b5d37cd4 (nodejs/node#63215, 2026-08-20) in
/// `lib/internal/errors/error_source.js`, and the band was predicted to close at
/// 26.8.0 as the first release that could carry it.
///
/// RE-VERIFIED 2026-09-01, now that the release exists — the boundary below is
/// measured rather than predicted, so this is no longer a stopgap. Running
/// `node --enable-source-maps -e 'try{require("assert").ok(false)}catch(e){console.log(e.constructor.name)}'`
/// on real binaries gives `TypeError` on 26.7.0 and `AssertionError` on 26.8.1.
///
/// The artifact published as v26.8.0 also gives `AssertionError`, with one caveat
/// worth stating rather than rounding off: that build self-reports
/// `v26.8.0-alpha.0.0.0`, which sorts BELOW `26.8.0` and so lands inside the band
/// this constant withholds on. So the clean measurement is 26.7.0 broken / 26.8.1
/// fixed, and 26.8.0 is bounded by the fix landing upstream before it was cut.
///
/// The trade-off is unchanged: withholding costs only stack-trace remapping (a
/// cosmetic loss), while injecting corrupts the TYPE of a thrown AssertionError,
/// which breaks `node:test` assertions outright.
fn source_maps_safe(node_version: &NodeVersion) -> bool {
    !(node_version.major() == 26 && *node_version < NodeVersion::new(26, 8, 0))
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
/// with the `webstorage` feature_matrix row). Only the banding tests call it
/// directly — production reaches the matrix via `webstorage_flag_needed`.
#[cfg(test)]
fn webstorage_supported(node_version: &NodeVersion) -> bool {
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
/// The ordinary spawn path and compiled launcher share this version-only half of
/// the decision. Use [`should_inject_experimental_webstorage`] at an argv-bearing
/// call site so an explicit user positive or negative is never duplicated or
/// overridden.
pub(crate) fn webstorage_flag_needed(node_version: &NodeVersion) -> bool {
    matches!(
        feature_matrix::feature("webstorage").mitigation_for(node_version),
        Some(Mitigation::Unflag(_))
    )
}

/// Whether this invocation should add `--experimental-webstorage` itself.
///
/// This is deliberately a narrow public predicate shared with `nub-launcher`:
/// it is true only in the feature matrix's `Unflag` band (Node 22.4 through
/// before 25) and only when neither argv nor `NODE_OPTIONS` already contains the
/// flag in either polarity. It says nothing about `--localstorage-file`; callers
/// must forward a user-supplied file normally and must never synthesize one.
pub fn should_inject_experimental_webstorage(
    node_version: &NodeVersion,
    user_argv: &[String],
    node_options: Option<&str>,
) -> bool {
    webstorage_flag_needed(node_version) && !user_supplied_webstorage_flag(user_argv, node_options)
}

/// The subset of nub's injected flags that is safe on the INHERITED `NODE_OPTIONS`
/// channel — the ones that cannot abort a descendant running an older Node.
///
/// `NODE_OPTIONS` reaches the whole process subtree, so a flag whose floor sits above
/// nub's 18.19 support floor can kill a descendant outright (Node rejects an unknown
/// flag there). Today exactly one injected flag clears that bar: `--enable-source-maps`,
/// which has existed since Node 12.12. Everything else is banded higher and stays on
/// argv — the `feature_matrix` unflags obviously, but ALSO
/// `--disable-warning=ExperimentalWarning`, whose 20.11 floor is above 18.19, so a
/// descendant on 18.x or 20.10 would abort on it. Losing it there costs only warning
/// suppression; keeping it would reintroduce the very hazard this channel split closes.
///
/// Derived by filtering [`compute_inject_flags`] against [`ALWAYS_INJECT`] rather than
/// listing flags again, so the source-map version carve-outs stay in one place.
pub fn node_options_safe_inject_flags(
    node_version: &NodeVersion,
    user_argv: &[String],
    node_options: Option<&str>,
) -> Vec<&'static str> {
    compute_inject_flags(node_version.clone(), user_argv, node_options, true, None)
        .into_iter()
        .filter(|flag| ALWAYS_INJECT.contains(flag))
        .collect()
}

/// Every ARGV-ONLY V8 flag nub should inject for this invocation, derived from the
/// feature matrix's [`super::feature_matrix::Mitigation::UnflagArgv`] rows.
///
/// Kept separate from [`compute_inject_flags`] on purpose. That function's output is
/// also the NODE_OPTIONS payload for the script-runner path, and its Stage-4
/// intersection is against `process.allowedNodeEnvironmentFlags` — a set that
/// describes NODE_OPTIONS eligibility and so excludes every flag returned here. Put
/// one of these through it and it is either dropped (probe available) or sent into a
/// NODE_OPTIONS that aborts on it (probe unavailable).
///
/// A flag the user already supplied on argv in EITHER polarity is skipped: nub never
/// double-adds, and never re-enables over a user negation. `NODE_OPTIONS` is
/// deliberately not consulted — Node rejects these flags there in both polarities, so
/// it is not a channel through which a user can express an opinion about them.
///
/// Each surviving flag is then confirmed against the actual binary via
/// [`super::discovery::accepts_argv_flag`]. That is this shape's equivalent of
/// `compute_inject_flags`' Stage-4 intersection: an open-ended band would otherwise
/// keep injecting a flag V8 has since removed, and an unknown `--js-*` is a hard
/// `node: bad option` startup abort on every augmented invocation.
pub fn argv_inject_flags(
    node_path: Option<&std::path::Path>,
    node_version: &NodeVersion,
    user_argv: &[String],
) -> Vec<&'static str> {
    let banded = feature_matrix::argv_unflag_flags_for(node_version);
    // Below every argv-unflag floor there is nothing to probe, so an out-of-band
    // Node pays no extra spawn at all.
    if banded.is_empty() {
        return Vec::new();
    }
    banded
        .into_iter()
        .filter(|flag| !user_supplied_either_polarity(user_argv, flag))
        // The removal backstop. `accepted_env_flags` cannot serve here (a
        // command-line-only V8 flag is absent from `allowedNodeEnvironmentFlags` by
        // construction), so this is its argv analog: drop a flag the running Node no
        // longer accepts instead of aborting it at startup. `None` — probe could not
        // run — preserves pure version-band behavior, matching Stage 4's contract.
        .filter(|flag| match node_path {
            // `None` — a Node nub itself provisioned or embedded, whose accepted flags
            // follow from its version — skips the probe and trusts the band, mirroring
            // why `accepted_env_flags` is skipped for a managed Node.
            None => true,
            Some(path) => super::discovery::accepts_argv_flag(path, flag).unwrap_or(true),
        })
        .collect()
}

/// Whether `user_argv` already carries `flag` as `--x` or as its `--no-x` negation.
fn user_supplied_either_polarity(user_argv: &[String], flag: &str) -> bool {
    let negated = format!("--no-{}", flag.trim_start_matches("--"));
    user_argv.iter().any(|arg| arg == flag || *arg == negated)
}

/// Whether the caller that injects experimental Web Storage must also tell its
/// preload to remove Node 22.4–24's throwing `localStorage` getter. This follows
/// the ordinary spawn contract: only an injected flag can create that getter, and
/// a user-supplied `--localstorage-file` leaves it usable instead.
///
/// Callers that place application arguments after the Node entry (such as the
/// compiled launcher) must pass an empty `user_argv`: those are not Node flags.
/// Such callers still pass inherited `NODE_OPTIONS`, where Node actually reads a
/// user-provided storage-file option.
pub fn should_neutralize_experimental_webstorage_localstorage(
    node_version: &NodeVersion,
    user_argv: &[String],
    node_options: Option<&str>,
) -> bool {
    should_inject_experimental_webstorage(node_version, user_argv, node_options)
        && !user_supplied_localstorage_file(user_argv, node_options)
}

/// Internal child-process signal consumed by the preload to neutralize Node's
/// throwing experimental-webstorage `localStorage` getter. This is plumbing, not
/// a user-facing environment option.
pub const NEUTRALIZE_LOCALSTORAGE_ENV: &str = "__NUB_NEUTRALIZE_LOCALSTORAGE";

/// Internal child-process signal listing the ARGV-only V8 flags nub injected on this
/// invocation, space-separated, so the preload can hide them from `process.execArgv`.
///
/// Necessary because these flags are exactly the ones Node REFUSES in `NODE_OPTIONS`,
/// and a great deal of real tooling forwards `process.execArgv` into a Worker or into a
/// child's `NODE_OPTIONS`. Node then rejects nub's own flag with
/// `ERR_WORKER_INVALID_EXEC_ARGV` and the build dies — which is exactly how a Next.js
/// 16 + Turbopack build broke. V8 parses these at startup, so removing them from
/// `execArgv` afterwards keeps the feature ON while restoring the `execArgv` a
/// plain-Node user would have seen. Plumbing, not a user-facing option.
pub const ARGV_ONLY_FLAGS_ENV: &str = "__NUB_ARGV_ONLY_FLAGS";

fn user_supplied_webstorage_flag(user_argv: &[String], node_options: Option<&str>) -> bool {
    let is_webstorage_flag = |token: &str| {
        matches!(
            token,
            "--experimental-webstorage" | "--no-experimental-webstorage"
        )
    };
    user_argv.iter().any(|arg| is_webstorage_flag(arg))
        || node_options.is_some_and(|options| {
            node_options_tokens(options)
                .iter()
                .any(|token| is_webstorage_flag(token))
        })
}

fn user_supplied_localstorage_file(user_argv: &[String], node_options: Option<&str>) -> bool {
    let is_localstorage_file =
        |token: &str| token == "--localstorage-file" || token.starts_with("--localstorage-file=");
    user_argv.iter().any(|arg| is_localstorage_file(arg))
        || node_options.is_some_and(|options| {
            node_options_tokens(options)
                .iter()
                .any(|token| is_localstorage_file(token))
        })
}

/// Split the small NODE_OPTIONS subset these webstorage gates inspect. Node accepts
/// quoted options, so `split_whitespace` misses a quoted `--no-…` token and splits
/// quoted storage paths. Keep this deliberately narrow: quotes and backslash
/// escapes are unwrapped only for recognition; the original NODE_OPTIONS string is
/// still forwarded to Node unchanged.
fn node_options_tokens(options: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in options.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else {
                token.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(ch),
        }
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
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
const MIN_TEST_COVERAGE_EXCLUDE: NodeVersion = NodeVersion::new(22, 5, 0);

/// Whether the target Node supports `--test-coverage-exclude` (argv or NODE_OPTIONS).
pub(crate) fn test_coverage_exclude_supported(node_version: &NodeVersion) -> bool {
    *node_version >= MIN_TEST_COVERAGE_EXCLUDE
}

/// Node started excluding the user's own test files from an
/// `--experimental-test-coverage` report by default in **23.5.0** — commit
/// `ea9a675f56`, "test_runner: exclude test files from coverage by default"
/// (nodejs/node#56060), which added the `coverageExcludeGlobs.length === 0`
/// fallback to `kDefaultPattern` in `lib/internal/test_runner/utils.js`.
///
/// It was **never backported to 22.x**, so this floor does NOT coincide with
/// `MIN_TEST_COVERAGE_EXCLUDE` (22.5.0): on 22.5–22.x and 23.0–23.4 the flag
/// exists while the default exclusion does not. That gap is the whole reason this
/// is a separate gate. nub pairs Node's default pattern with its own runtime
/// exclude to stop the exclude from switching that default off — so on a Node that
/// has no such default, injecting the pattern would EXCLUDE test files stock Node
/// includes, breaking parity in the opposite direction.
///
/// Verified empirically on the logic.js/logic.test.js fixture: 18.19.0, 22.15.0,
/// 22.16.0, 22.23.1, 22.23.2, 23.0.0, 23.3.0 and 23.4.0 all report the test file;
/// 23.5.0, 23.6.0, 23.11.0, 24.0.0, 24.17.0, 25.9.0, 26.3.0 and 26.7.0 all exclude
/// it. 22.23.2 is the newest 22.x published, which is what rules out a backport.
const MIN_TEST_COVERAGE_DEFAULT_EXCLUSION: NodeVersion = NodeVersion::new(23, 5, 0);

/// Whether the target Node applies its own default test-file coverage exclusion.
pub(crate) fn test_coverage_default_exclusion_applied(node_version: &NodeVersion) -> bool {
    *node_version >= MIN_TEST_COVERAGE_DEFAULT_EXCLUSION
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
/// (Webstorage's `--experimental-webstorage` stays outside this static set because
/// argv-bearing callers use [`should_inject_experimental_webstorage`] to respect
/// an explicit user positive or negative without creating duplicates.)
/// `accepted_env_flags`: the running Node binary's actual
/// `process.allowedNodeEnvironmentFlags` set (probed + cached in
/// [`super::discovery::accepted_env_flags`]). When `Some`, the computed inject set
/// is intersected with it in Stage 4 so nub never injects a flag THIS Node rejects
/// — the self-correcting guard for open-ended `Unflag` bands (a flag Node
/// hard-removes on a later major, e.g. `--experimental-permission` → `--permission`
/// at 24.0). `None` (probe unavailable) preserves pure version-band behavior.
pub fn compute_inject_flags(
    node_version: NodeVersion,
    user_argv: &[String],
    node_options: Option<&str>,
    show_warnings: bool,
    accepted_env_flags: Option<&std::collections::BTreeSet<String>>,
) -> Vec<&'static str> {
    // Stage 1: compute the would-inject set.
    let mut flags: Vec<&str> = Vec::new();

    for &flag in ALWAYS_INJECT {
        // --enable-source-maps is withheld below Node 26.8 on the 26.x line (see
        // `source_maps_safe`): there it turns a no-message AssertionError into a
        // TypeError.
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
        // Skip the webstorage flag here: argv-bearing callers own its injection
        // through `should_inject_experimental_webstorage`, gated on the same
        // matrix band and suppressed when the user already supplied either
        // polarity. Putting it in this static set would bypass that suppression.
        if flag == "--experimental-webstorage" {
            continue;
        }
        flags.push(flag);
    }

    // Stage 2: parse user opt-outs from argv and NODE_OPTIONS.
    let user_negations = collect_negations(user_argv, node_options);

    // Stage 3: subtract.
    flags.retain(|flag| !user_negations.iter().any(|neg| is_negation_of(neg, flag)));

    // Stage 4: intersect with the binary's ACTUAL accepted-flag set, when known.
    // Version bands say what nub WANTS; the probe says what THIS Node binary
    // ACCEPTS. Node hard-removes some experimental flags on later majors (e.g.
    // `--experimental-policy`, `--experimental-permission` → `--permission` at 24.0),
    // so an open-ended `Unflag` band would otherwise inject a "bad option" that
    // aborts Node at startup. Dropping any flag the binary doesn't accept makes
    // injection self-correcting — no nub release needed when Node removes a flag.
    // `None` (probe unavailable — rare; means the binary isn't runnable, in which
    // case the spawn fails anyway) leaves the version-band set untouched: no
    // regression. Compare on the flag NAME (token before `=`), since value-bearing
    // flags like `--disable-warning=ExperimentalWarning` appear in the accepted set
    // as the bare name `--disable-warning`.
    if let Some(accepted) = accepted_env_flags {
        flags.retain(|flag| {
            let name = flag.split_once('=').map_or(*flag, |(n, _)| n);
            accepted.contains(name)
        });
    }

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
///     unflags it. This floor gates the LOWER bound only (`[0, floor)` is "bad
///     option" territory). It does NOT bound the top: Node may KEEP a flag as an
///     accepted no-op indefinitely (`--experimental-fetch`, `--experimental-modules`)
///     OR HARD-REMOVE it on a later major (`--experimental-policy`,
///     `--experimental-permission` → `--permission` at 24.0), which no static floor
///     can predict. The upper-bound guard is dynamic, not a table:
///     `compute_inject_flags`' Stage 4 intersects the inject set with the binary's
///     probed `allowedNodeEnvironmentFlags` (see `super::discovery::accepted_env_flags`),
///     dropping any flag the running Node rejects. So a single floor per flag
///     remains sufficient HERE, with the runtime probe covering removal.
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
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, false, None);
        assert!(flags.contains(&"--disable-warning=ExperimentalWarning"));
        assert!(flags.contains(&"--enable-source-maps"));
    }

    #[test]
    fn injects_unflag_set_on_22_15() {
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, false, None);
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
            compute_inject_flags(v(18, 19, 0), &[], None, false, None)
                .contains(&"--experimental-vm-modules")
        );
        assert!(
            compute_inject_flags(v(26, 2, 0), &[], None, false, None)
                .contains(&"--experimental-vm-modules")
        );
    }

    #[test]
    fn eventsource_skips_the_21x_hole() {
        // EventSource landed at 22.3.0 + 20.18.0 backport; never shipped on 21.x.
        // Injecting on 21.x is a "bad option" crash — the highest-stakes boundary here.
        let yes = "--experimental-eventsource";
        assert!(!compute_inject_flags(v(20, 17, 0), &[], None, false, None).contains(&yes));
        assert!(compute_inject_flags(v(20, 18, 0), &[], None, false, None).contains(&yes));
        // The hole: must NOT inject anywhere on the 21.x line.
        assert!(
            !compute_inject_flags(v(21, 0, 0), &[], None, false, None).contains(&yes),
            "must NOT inject --experimental-eventsource on 21.0 (flag never existed there → crash)"
        );
        assert!(!compute_inject_flags(v(22, 2, 0), &[], None, false, None).contains(&yes));
        assert!(compute_inject_flags(v(22, 3, 0), &[], None, false, None).contains(&yes));
        assert!(compute_inject_flags(v(26, 2, 0), &[], None, false, None).contains(&yes));
    }

    #[test]
    fn sqlite_injected_only_in_the_two_flagged_bands() {
        // node:sqlite: flag added 22.5.0, unflagged 22.13.0 (22.x) and 23.4.0 (23.x).
        // Inject only where the flag exists AND is still required.
        let sql = "--experimental-sqlite";
        assert!(!compute_inject_flags(v(22, 4, 0), &[], None, false, None).contains(&sql)); // flag absent
        assert!(compute_inject_flags(v(22, 5, 0), &[], None, false, None).contains(&sql)); // band 1 floor
        assert!(compute_inject_flags(v(22, 12, 0), &[], None, false, None).contains(&sql));
        assert!(!compute_inject_flags(v(22, 13, 0), &[], None, false, None).contains(&sql)); // unflagged on 22.x
        assert!(compute_inject_flags(v(23, 3, 0), &[], None, false, None).contains(&sql)); // band 2
        assert!(!compute_inject_flags(v(23, 4, 0), &[], None, false, None).contains(&sql)); // unflagged on 23.x
        assert!(!compute_inject_flags(v(24, 0, 0), &[], None, false, None).contains(&sql)); // unflagged everywhere after
    }

    #[test]
    fn websocket_injected_only_on_flag_gated_band() {
        // WebSocket global is flag-gated on [20.10.0, 22.0.0): exists on 20.10+ and all
        // 21.x, default-on from 22.0.0. Below 20.10 the flag doesn't exist ("bad option").
        let ws = "--experimental-websocket";
        assert!(!compute_inject_flags(v(20, 9, 0), &[], None, false, None).contains(&ws));
        assert!(compute_inject_flags(v(20, 10, 0), &[], None, false, None).contains(&ws));
        assert!(compute_inject_flags(v(21, 5, 0), &[], None, false, None).contains(&ws)); // all of 21.x
        assert!(!compute_inject_flags(v(22, 0, 0), &[], None, false, None).contains(&ws)); // default-on
    }

    #[test]
    fn wasm_modules_injected_only_in_the_two_flagged_bands() {
        // Wasm ES-module imports: flag exists since Node 12 (below the floor),
        // default-on at 24.5.0 (24.x) and 22.19.0 (22.x) via #57038; never
        // default-on on the 23.x line (EOL before the backport). Inject on
        // [18.19, 22.19) ∪ [23.0, 24.5).
        let w = "--experimental-wasm-modules";
        assert!(compute_inject_flags(v(18, 19, 0), &[], None, false, None).contains(&w)); // floor
        assert!(compute_inject_flags(v(22, 13, 0), &[], None, false, None).contains(&w));
        assert!(compute_inject_flags(v(22, 18, 0), &[], None, false, None).contains(&w));
        assert!(!compute_inject_flags(v(22, 19, 0), &[], None, false, None).contains(&w)); // default-on 22.x
        assert!(compute_inject_flags(v(23, 2, 0), &[], None, false, None).contains(&w)); // 23.x stays flagged
        assert!(compute_inject_flags(v(24, 4, 0), &[], None, false, None).contains(&w));
        assert!(!compute_inject_flags(v(24, 5, 0), &[], None, false, None).contains(&w)); // default-on 24.x
        assert!(!compute_inject_flags(v(26, 0, 0), &[], None, false, None).contains(&w));
    }

    #[test]
    fn import_text_injected_on_both_flag_bearing_lines() {
        // `--experimental-import-text` was added in Node 26.5.0 (#62300) and backported
        // to 24.19.0; it is not default-on through Node 27. Inject on both bands, never
        // on a release that lacks the flag (there it is a "bad option" abort): below
        // 24.19.0, and the whole 25.x line, which ended before the backport. Also verify
        // it snips out of an inherited NODE_OPTIONS on a child below the floor.
        let it = "--experimental-import-text";
        assert!(!compute_inject_flags(v(24, 18, 1), &[], None, false, None).contains(&it));
        assert!(compute_inject_flags(v(24, 19, 0), &[], None, false, None).contains(&it));
        assert!(!compute_inject_flags(v(26, 4, 0), &[], None, false, None).contains(&it));
        assert!(compute_inject_flags(v(26, 5, 0), &[], None, false, None).contains(&it));
        assert!(compute_inject_flags(v(27, 0, 0), &[], None, false, None).contains(&it));
        // A user opt-out subtracts it.
        let argv = vec!["--no-experimental-import-text".to_string()];
        assert!(!compute_inject_flags(v(26, 5, 0), &argv, None, false, None).contains(&it));
        // Inherited NODE_OPTIONS: stripped below the floor, kept at/above it.
        assert_eq!(strip_unsupported_node_options(it, &v(24, 18, 1)), "");
        assert_eq!(strip_unsupported_node_options(it, &v(24, 19, 0)), it);
        assert_eq!(strip_unsupported_node_options(it, &v(26, 5, 0)), it);
    }

    #[test]
    fn accepted_env_flags_intersection_guards_removed_flags() {
        // The Stage-4 guard: an open-ended `Unflag` band (import-text's upper band is
        // `[26.5, ∞)`) would keep injecting a flag Node later HARD-REMOVES (as it did with
        // `--experimental-permission` → `--permission` at 24.0), aborting startup with
        // "bad option". Intersecting with the binary's probed accepted-flag set drops
        // exactly that flag. Model a future Node that removed import-text but still
        // accepts the rest.
        let it = "--experimental-import-text";
        let mut accepted: std::collections::BTreeSet<String> = [
            "--enable-source-maps",
            "--disable-warning",
            "--experimental-vm-modules",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // import-text ABSENT from the accepted set → dropped despite the band wanting it.
        assert!(
            !compute_inject_flags(v(27, 0, 0), &[], None, false, Some(&accepted)).contains(&it)
        );
        // enable-source-maps present as a bare name → kept even though nub may inject a
        // value-bearing form for other flags (name-based match).
        assert!(
            compute_inject_flags(v(27, 0, 0), &[], None, false, Some(&accepted))
                .contains(&"--enable-source-maps")
        );
        // Value-bearing flag matches on its NAME: `--disable-warning=…` is kept because
        // the accepted set carries the bare `--disable-warning`.
        assert!(
            compute_inject_flags(v(22, 15, 0), &[], None, false, Some(&accepted))
                .contains(&"--disable-warning=ExperimentalWarning")
        );
        // Once the accepted set (re)includes import-text, it is injected again.
        accepted.insert(it.to_string());
        assert!(compute_inject_flags(v(27, 0, 0), &[], None, false, Some(&accepted)).contains(&it));
    }

    #[test]
    fn accepted_env_flags_none_preserves_version_band() {
        // `None` (probe unavailable) is a pure pass-through: identical to the
        // pre-guard version-band behavior, so a failed probe never regresses.
        for ver in [v(22, 15, 0), v(26, 5, 0), v(27, 0, 0)] {
            assert_eq!(
                compute_inject_flags(ver.clone(), &[], None, false, None),
                compute_inject_flags(ver.clone(), &[], None, false, None),
            );
        }
    }

    /// Real-Node invariant: for the host Node, EVERY flag nub wants to inject at that
    /// version IS accepted by that Node (`process.allowedNodeEnvironmentFlags`). This
    /// is the property the whole design leans on — nub only injects envvar-allowed
    /// flags — so the Stage-4 intersection drops NOTHING on a current, supported Node
    /// (it only ever bites a FUTURE Node that removed a flag). If this fails, nub is
    /// injecting a flag the running Node rejects (a real startup-crash bug), not a
    /// test artifact. Skips when no `node` is discoverable.
    #[test]
    fn host_node_accepts_every_injected_flag() {
        let Ok(node) = crate::node::discovery::discover_node(std::path::Path::new(".")) else {
            eprintln!("skipping: no node discoverable");
            return;
        };
        let Some(accepted) = crate::node::discovery::accepted_env_flags(node.path.as_std_path())
        else {
            eprintln!("skipping: could not probe {}", node.path);
            return;
        };
        let with = compute_inject_flags(node.version.clone(), &[], None, false, Some(&accepted));
        let without = compute_inject_flags(node.version.clone(), &[], None, false, None);
        assert_eq!(
            with,
            without,
            "the accepted-flag intersection dropped a flag on host Node v{} — nub wants a \
             flag this Node does not accept: {:?}",
            node.version,
            without
                .iter()
                .filter(|f| !with.contains(*f))
                .collect::<Vec<_>>()
        );
    }

    /// The CONVERSE invariant, for `--experimental-import-text` specifically: if the host
    /// Node KNOWS the flag, nub must inject it at that version. The preload's step-aside
    /// (`NATIVE_IMPORT_TEXT` in preload-common.cjs) keys off exactly this accepted-flag
    /// set, so a release that knows the flag but sits outside the feature-matrix band
    /// hands `with { type: "text" }` to Node's default loader with the feature off —
    /// ERR_UNKNOWN_FILE_EXTENSION. That is #688: Node backported the flag to 24.19.0 while
    /// the band still started at 26.5.0. This guard turns the next backport into a red
    /// test instead of a red trunk. Skips when no `node` is discoverable.
    #[test]
    fn host_node_that_knows_import_text_gets_it_injected() {
        let it = "--experimental-import-text";
        let Ok(node) = crate::node::discovery::discover_node(std::path::Path::new(".")) else {
            eprintln!("skipping: no node discoverable");
            return;
        };
        let Some(accepted) = crate::node::discovery::accepted_env_flags(node.path.as_std_path())
        else {
            eprintln!("skipping: could not probe {}", node.path);
            return;
        };
        if !accepted.contains(it) {
            // This Node has no native text imports, so the preload never steps aside
            // and nub's polyfill owns them — nothing to guard.
            eprintln!("skipping: host Node v{} does not know {it}", node.version);
            return;
        }
        assert!(
            compute_inject_flags(node.version.clone(), &[], None, false, Some(&accepted))
                .contains(&it),
            "host Node v{} accepts {it} — so the preload steps aside to Node's native text \
             translator — but the feature-matrix import-text bands do not inject it there, \
             leaving text imports broken. Widen the bands to cover this release.",
            node.version
        );
    }

    #[test]
    fn shadow_realm_never_injected() {
        // ShadowRealm is DELIBERATELY not auto-unflagged (the harmony-flag policy):
        // `--experimental-shadow-realm` implies V8's `--harmony-shadow-realm`, which
        // changes the isolate's V8-flag hash and crashes embedded Node booting from a
        // context snapshot (Electron) in CreateEnvironment — before any preload JS,
        // so a self-disable can't catch it (#246). Never inject it, at any version.
        // The categorical guard lives in feature_matrix (no_v8_harmony_flag_in_unflag_set).
        let s = "--experimental-shadow-realm";
        assert!(!compute_inject_flags(v(18, 19, 0), &[], None, false, None).contains(&s));
        assert!(!compute_inject_flags(v(22, 19, 0), &[], None, false, None).contains(&s));
        assert!(!compute_inject_flags(v(26, 0, 0), &[], None, false, None).contains(&s));
    }

    #[test]
    fn user_opt_out_via_argv() {
        let argv = vec!["--no-experimental-vm-modules".to_string()];
        let flags = compute_inject_flags(v(22, 15, 0), &argv, None, false, None);
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
            None,
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
            !compute_inject_flags(v(22, 15, 0), &argv, None, false, None)
                .contains(&"--enable-source-maps"),
            "user --no-enable-source-maps (argv) must suppress nub's always-inject"
        );
        assert!(
            !compute_inject_flags(
                v(22, 15, 0),
                &[],
                Some("--no-enable-source-maps"),
                false,
                None
            )
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
            None,
        );
        assert!(
            flags.contains(&"--disable-warning=ExperimentalWarning"),
            "nub injects its own warning suppression alongside the user's different value"
        );
    }

    #[test]
    fn show_warnings_suppresses_warning_flag() {
        let flags = compute_inject_flags(v(22, 15, 0), &[], None, true, None);
        assert!(!flags.contains(&"--disable-warning=ExperimentalWarning"));
        assert!(flags.contains(&"--enable-source-maps"));
    }

    #[test]
    fn floor_injects_only_universally_safe_flags() {
        // At 20.0.0: --enable-source-maps and vm-modules (whole-floor) inject, but the
        // version-gated entries do not — sqlite/eventsource/websocket flags don't exist
        // here ("bad option"), and --disable-warning is below its 20.11 floor.
        let flags = compute_inject_flags(v(20, 0, 0), &[], None, false, None);
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
            let flags = compute_inject_flags(ver.clone(), &[], None, false, None);
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
            let flags = compute_inject_flags(ver.clone(), &[], None, false, None);
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
    fn shared_webstorage_injection_predicate_honors_bands_and_user_intent() {
        // Below the matrix floor the flag is invalid; from 25 it is native, so
        // neither band may add it. The closed 22.4–<25 Unflag band alone does.
        for version in [v(18, 19, 0), v(22, 3, 0), v(25, 0, 0), v(26, 2, 0)] {
            assert!(
                !should_inject_experimental_webstorage(&version, &[], None),
                "must not inject on {version:?}"
            );
        }
        for version in [v(22, 4, 0), v(22, 15, 0), v(24, 99, 0)] {
            assert!(
                should_inject_experimental_webstorage(&version, &[], None),
                "must inject on {version:?}"
            );
        }

        let version = v(22, 15, 0);
        for (argv, node_options) in [
            (vec!["--experimental-webstorage".to_string()], None),
            (vec!["--no-experimental-webstorage".to_string()], None),
            (vec![], Some("--experimental-webstorage")),
            (vec![], Some("--no-experimental-webstorage")),
        ] {
            assert!(
                !should_inject_experimental_webstorage(&version, &argv, node_options),
                "an explicit user polarity must suppress launcher injection"
            );
        }
        assert!(should_inject_experimental_webstorage(
            &version,
            &["--experimental-webstorage-extra".to_string()],
            None,
        ));
    }

    #[test]
    fn webstorage_node_options_recognizes_quoted_flags_and_storage_files() {
        let version = v(22, 15, 0);
        for options in [
            "'--experimental-webstorage'",
            "\"--experimental-webstorage\"",
            "'--no-experimental-webstorage'",
            "\"--no-experimental-webstorage\"",
        ] {
            assert!(
                !should_inject_experimental_webstorage(&version, &[], Some(options)),
                "a quoted user flag must suppress injection: {options}"
            );
        }

        for options in [
            "--localstorage-file='/tmp/nub storage'",
            "--localstorage-file=\"/tmp/nub storage\"",
        ] {
            assert!(
                should_inject_experimental_webstorage(&version, &[], Some(options)),
                "a storage file keeps sessionStorage injection enabled: {options}"
            );
            assert!(
                !should_neutralize_experimental_webstorage_localstorage(
                    &version,
                    &[],
                    Some(options),
                ),
                "a quoted storage path must keep localStorage usable: {options}"
            );
        }
    }

    #[test]
    fn webstorage_neutralization_only_follows_an_injected_flag_without_a_file() {
        let version = v(22, 15, 0);
        assert!(
            should_neutralize_experimental_webstorage_localstorage(&version, &[], None),
            "the flag-needed band without a file has Node's throwing getter"
        );
        assert!(should_inject_experimental_webstorage(
            &version,
            &[],
            Some("--localstorage-file=/tmp/store")
        ));
        assert!(
            !should_neutralize_experimental_webstorage_localstorage(
                &version,
                &[],
                Some("--localstorage-file=/tmp/store"),
            ),
            "a real NODE_OPTIONS file makes localStorage usable"
        );
        assert!(
            !should_neutralize_experimental_webstorage_localstorage(
                &version,
                &["--localstorage-file".to_string(), "/tmp/store".to_string()],
                None,
            ),
            "ordinary spawn argv preserves its space-separated file support"
        );
        assert!(
            !should_neutralize_experimental_webstorage_localstorage(
                &version,
                &[],
                Some("--no-experimental-webstorage"),
            ),
            "a user disable means this call did not inject the flag"
        );
        assert!(
            !should_neutralize_experimental_webstorage_localstorage(&v(25, 0, 0), &[], None),
            "native Web Storage has no injected throwing getter"
        );
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
    fn test_coverage_default_exclusion_gated_to_23_5_and_never_backported_to_22() {
        // The two coverage gates do NOT share a floor, and treating them as one is
        // what breaks parity on 22.x: there the flag exists but Node has no default
        // test-file exclusion, so pairing nub's runtime exclude with Node's default
        // pattern would hide test files stock Node reports. 22.23.2 is the newest
        // 22.x published and still has no default — the line never got the backport.
        assert!(!test_coverage_default_exclusion_applied(&v(18, 19, 0)));
        assert!(!test_coverage_default_exclusion_applied(&v(22, 15, 0)));
        assert!(!test_coverage_default_exclusion_applied(&v(22, 23, 2)));
        assert!(!test_coverage_default_exclusion_applied(&v(23, 4, 0)));
        assert!(test_coverage_default_exclusion_applied(&v(23, 5, 0)));
        assert!(test_coverage_default_exclusion_applied(&v(24, 0, 0)));
        assert!(test_coverage_default_exclusion_applied(&v(26, 7, 0)));

        // The whole band between the two floors has the flag and not the default.
        for version in [v(22, 5, 0), v(22, 23, 2), v(23, 4, 0)] {
            assert!(
                test_coverage_exclude_supported(&version)
                    && !test_coverage_default_exclusion_applied(&version),
                "{version:?} must be inside the flag-without-default band"
            );
        }
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
    fn source_maps_withheld_across_the_whole_26_x_regression_band() {
        // Every Node 26.x below 26.8 regresses (nodejs/node#63169): with
        // --enable-source-maps, a no-message assert.ok(false) throws TypeError
        // [ERR_INVALID_ARG_TYPE] instead of AssertionError. The band's EDGES are
        // what this pins — 25.9.0 below it, 26.8.0 (first release that can carry
        // the upstream fix b5d37cd4) and 27.0.0 above it.
        for ver in [v(24, 0, 0), v(25, 9, 0), v(26, 8, 0), v(27, 0, 0)] {
            assert!(
                source_maps_safe(&ver),
                "source maps must be safe to inject on {ver:?}"
            );
            assert!(
                compute_inject_flags(ver.clone(), &[], None, false, None)
                    .contains(&"--enable-source-maps"),
                "--enable-source-maps must inject on {ver:?}"
            );
        }
        // The affected band: 26.0.0 through 26.7.x, inclusive.
        for ver in [v(26, 0, 0), v(26, 2, 0), v(26, 7, 0)] {
            assert!(
                !source_maps_safe(&ver),
                "source maps must be withheld on {ver:?}"
            );
            assert!(
                !compute_inject_flags(ver.clone(), &[], None, false, None)
                    .contains(&"--enable-source-maps"),
                "--enable-source-maps must NOT inject on {ver:?} (assert→TypeError regression)"
            );
        }
    }
}
