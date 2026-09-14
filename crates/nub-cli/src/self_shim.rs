//! nub self-shim — honor `packageManager: "nub@x.y.z"` by provisioning that exact
//! nub and delegating (`exec`) to it, corepack's model applied to nub itself.
//!
//! When a project's workspace-root manifest pins a *different* exact nub than the
//! running one, a PM mutating verb (`install`/`add`/…) provisions the pinned nub
//! from nub's own release channel and replaces the process with it, so every
//! contributor and CI runs byte-identical package-manager behavior. This module
//! owns the pure *decision*; `cli::provision_self` / `cli::delegate_to_self` own
//! the fetch-verify-exec.
//!
//! HOT PATH: the flagship `nub <file>` runner, `run`/`watch`, `nubx`, `upgrade`,
//! and `node` never reach a manifest read — [`delegate_target`] gates on the verb
//! allowlist FIRST (a pure string compare). Only a delegating PM verb reads the
//! workspace-root pin, a read those verbs already make downstream.
//!
//! LOOP-SAFETY: delegation fires only on an EXACT `nub@X.Y.Z` that differs from
//! the running version, with the re-entry guard unset and the opt-out off. The
//! child runs with `__NUB_SELF_DISPATCHED=<version>`, whose mere presence
//! suppresses ALL further delegation ([`decide`] returns `Continue`
//! unconditionally when it is set) — no exec loop is reachable. Exact-version-only
//! pins mean a spec can never resolve to a different concrete string across the
//! boundary (corepack enforces the same for the field).
//!
//! INTEGRITY: the delegated artifact is nub's own per-platform release binary,
//! SHA-256-verified against the published `.sha256` sidecar before it runs (see
//! `cli::provision_self`). nub's `packageManager` self-pin carries NO `+sha512`
//! (unlike a pnpm/yarn pin, whose hash covers a platform-independent npm tarball),
//! and one hash could not cover 8 per-platform binaries — so the release-channel
//! checksum is the integrity anchor, not the pin.

use std::path::{Path, PathBuf};

/// Set on a delegated child so it never re-delegates — the exec-loop guard,
/// keyed to the resolved concrete version. Internal `__NUB_` plumbing (brand-
/// exempt), never a user knob.
pub(crate) const SELF_DISPATCHED_ENV: &str = "__NUB_SELF_DISPATCHED";

/// The user-facing opt-out: a falsey value disables auto-delegation tree-wide and
/// is inherited by every descendant. Positive-default spelling (modelled on
/// `NODE_COMPAT`) — only an explicit falsey value turns the feature off. `NUB_*`
/// is a sanctioned PM knob prefix, as `NUB_CACHE_DIR` is.
pub(crate) const SELF_SHIM_ENV: &str = "NUB_SELF_SHIM";

/// The PM mutating verbs that delegate to a pinned nub — the set whose deliverable
/// is a *committed* artifact (lockfile + store/linker layout), where "everyone
/// runs the same nub" is a checkable property that lockfile-compat alone doesn't
/// pin. Everything else is exempt: the file runner and `run`/`watch` (non-
/// committed output, latency-critical), `nubx`/`dlx` (ephemeral), `upgrade`
/// (nub's self-update — a category error to delegate), `node` (orthogonal to the
/// nub version), and read-only queries.
fn is_delegating_verb(verb: &str) -> bool {
    // `install`/`i`/`ci` are top-level SUBCOMMANDS (not ENGINE_VERBS); the rest
    // resolve through the alias-aware engine registry so `a`→add, `rm`→remove,
    // `up`→update all count. `upgrade` is deliberately NOT an `update` alias in
    // nub (it is the self-update), so `lookup_verb` never maps it here.
    if matches!(verb, "install" | "i" | "ci") {
        return true;
    }
    matches!(
        crate::pm_engine::lookup_verb(verb).map(|v| v.canonical),
        Some("add" | "remove" | "update" | "import" | "dedupe")
    )
}

/// Whether auto-delegation is opted out via `NUB_SELF_SHIM` — a falsey value
/// (`0`/`false`/`no`/`off`, case-insensitive). Unset or any other value = enabled.
fn opted_out() -> bool {
    match std::env::var(SELF_SHIM_ENV) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// The delegation decision — computed purely from its inputs (no I/O) so it is
/// exhaustively unit-testable and the loop-safety argument is inspectable.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Run in-process: the hot path, no pin, a matching pin, or a re-entry.
    Continue,
    /// Provision + exec this exact nub version.
    Delegate(semver::Version),
    /// Print this one-line stderr notice, then run in-process.
    Notice(String),
}

/// The loop-safe core decision. `declared` is the workspace-root `packageManager`
/// field's `(name, version)` (with `+sha512` build-metadata already stripped);
/// `running` is the running nub's version; `guard_set` is whether
/// `__NUB_SELF_DISPATCHED` is present; `opted_out` is the `NUB_SELF_SHIM` opt-out;
/// `platform_supported` is whether nub publishes a release build for this OS/arch.
/// The caller has already confirmed the verb is a delegating one.
fn decide(
    declared: Option<(String, Option<String>)>,
    running: &semver::Version,
    guard_set: bool,
    opted_out: bool,
    platform_supported: bool,
) -> Plan {
    // No pin, or a name-only pin with no version → nothing to honor.
    let Some((name, Some(pinned_raw))) = declared else {
        return Plan::Continue;
    };
    // Only a nub self-pin is our concern; a foreign PM pin is the engine's.
    if name != "nub" {
        return Plan::Continue;
    }
    // Re-entry guard: presence-first and UNCONDITIONAL. A delegated child — and
    // any nested delegating verb beneath it — never re-delegates. Checked before
    // parsing so no input shape can bypass the primary loop-safety stop.
    if guard_set {
        return Plan::Continue;
    }
    // Exact-version-only (corepack mandates it for the field; also loop-safety —
    // a range/tag could resolve to a different concrete string across the exec).
    let Ok(pinned) = semver::Version::parse(&pinned_raw) else {
        return Plan::Notice(format!(
            "nub: packageManager pins nub@{pinned_raw}, which isn't an exact version — \
             running with your installed nub@{running}. Pin an exact version \
             (e.g. nub@{running}) to auto-provision the pinned nub."
        ));
    };
    // Matching pin → the pinning user's hot path: a field read + a compare, then
    // continue IN-PROCESS. Never respawn when we already are the pinned version
    // (this is the trap the `+sha512` strip in the field reader exists to avoid).
    if pinned == *running {
        return Plan::Continue;
    }
    // A genuine mismatch, but auto-delegation is opted out → coherent notice.
    if opted_out {
        return Plan::Notice(format!(
            "nub: this project pins nub@{pinned}; you're running nub@{running}. \
             Auto-provisioning is disabled (NUB_SELF_SHIM=0) — running with your \
             installed nub."
        ));
    }
    // No published build for this OS/arch → graceful in-process fallback, NOT a
    // hard error. A default-on provision that can't succeed on a whole platform
    // (e.g. one with no release artifact yet) must not brick every install there.
    if !platform_supported {
        return Plan::Notice(format!(
            "nub: this project pins nub@{pinned}, but nub publishes no build for this \
             platform ({}/{}) — running with your installed nub@{running}. Set \
             NUB_SELF_SHIM=0 to silence.",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }
    Plan::Delegate(pinned)
}

/// Resolve the workspace-root pin and decide whether a delegating PM verb should
/// hand off to a pinned nub. Returns the exact version to provision + exec, or
/// `None` to continue in-process (printing any coherent notice as a side effect).
/// `rest` is the resolved subcommand argv (`rest[0]` is the verb); `cwd_override`
/// is the `--cwd` value (a relative path resolved against the current dir).
///
/// The verb-allowlist gate runs FIRST — a non-delegating verb returns with zero
/// I/O, so the flagship `nub <file>` / `run` / `nubx` paths never read the
/// manifest.
pub(crate) fn delegate_target(rest: &[String], cwd_override: Option<&Path>) -> Option<String> {
    let verb = rest.first()?;
    if !is_delegating_verb(verb) {
        return None; // hot path: a pure string compare, no syscalls
    }
    let running = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let guard_set = std::env::var_os(SELF_DISPATCHED_ENV).is_some();
    let effective = effective_cwd(cwd_override);
    // The one manifest read — reached only for a delegating verb, which reads the
    // root manifest downstream anyway (the field reader hits ROOT_MANIFEST_CACHE).
    // The `packageManager` field ONLY (never the devEngines range-of-intent) drives
    // the shim — matching the exact-pin channel corepack enforces.
    let declared = nub_core::pm::resolve::declared_package_manager_field(&effective);
    let platform_supported = crate::cli::platform_target().is_some();
    match decide(
        declared,
        &running,
        guard_set,
        opted_out(),
        platform_supported,
    ) {
        Plan::Continue => None,
        Plan::Notice(msg) => {
            eprintln!("{msg}");
            None
        }
        Plan::Delegate(version) => Some(version.to_string()),
    }
}

/// The dir to read the workspace-root pin from: the `--cwd` override resolved
/// against the process cwd (an absolute override wins; a relative one joins), or
/// the process cwd. Computed WITHOUT mutating the process cwd, so a delegated
/// child re-applies `--cwd` from the original directory exactly once.
fn effective_cwd(cwd_override: Option<&Path>) -> PathBuf {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match cwd_override {
        Some(dir) => base.join(dir),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn delegating_allowlist_covers_the_mutating_verbs_and_their_aliases() {
        // The eight families the shim delegates, plus the aliases users type.
        for verb in [
            "install",
            "i",
            "ci",
            "add",
            "a",
            "remove",
            "rm",
            "uninstall",
            "un",
            "update",
            "up",
            "import",
            "dedupe",
        ] {
            assert!(is_delegating_verb(verb), "{verb} should delegate");
        }
    }

    #[test]
    fn exempt_verbs_never_delegate() {
        // The hot path + every non-committed / self-update / orthogonal surface.
        // `upgrade` is the sharpest: it must NOT be treated as an `update` alias.
        for verb in [
            "run",
            "watch",
            "exec",
            "nubx",
            "dlx",
            "x",
            "upgrade",
            "node",
            "pm",
            "agent",
            "list",
            "ls",
            "why",
            "outdated",
            "help",
            "app.ts",
            "./build.js",
            "prune",
            "rebuild",
        ] {
            assert!(!is_delegating_verb(verb), "{verb} must not delegate");
        }
    }

    // decide()'s trailing args: (guard_set, opted_out, platform_supported). The
    // common case is (false, false, true) — not a guard, not opted out, a real
    // platform build exists.
    #[test]
    fn no_pin_or_foreign_pin_continues() {
        assert_eq!(
            decide(None, &v("0.2.9"), false, false, true),
            Plan::Continue
        );
        assert_eq!(
            decide(Some(("nub".into(), None)), &v("0.2.9"), false, false, true),
            Plan::Continue,
            "a name-only nub pin has no version to honor"
        );
        assert_eq!(
            decide(
                Some(("pnpm".into(), Some("9.1.0".into()))),
                &v("0.2.9"),
                false,
                false,
                true
            ),
            Plan::Continue,
            "a foreign PM pin is the engine's concern, not the self-shim's"
        );
    }

    #[test]
    fn matching_pin_is_a_silent_no_op() {
        // The `+sha512` strip happens in the field reader upstream — decide sees the
        // bare version. A stripped self-pin equal to the running version must NOT
        // respawn (the regression the whole feature would otherwise ship).
        assert_eq!(
            decide(
                Some(("nub".into(), Some("0.2.9".into()))),
                &v("0.2.9"),
                false,
                false,
                true
            ),
            Plan::Continue
        );
    }

    #[test]
    fn exact_mismatch_delegates() {
        assert_eq!(
            decide(
                Some(("nub".into(), Some("0.1.0".into()))),
                &v("0.2.9"),
                false,
                false,
                true
            ),
            Plan::Delegate(v("0.1.0"))
        );
    }

    #[test]
    fn reentry_guard_suppresses_delegation_unconditionally() {
        // We ARE a delegated child: even a differing exact pin must continue, or
        // the child would re-delegate forever.
        assert_eq!(
            decide(
                Some(("nub".into(), Some("0.1.0".into()))),
                &v("0.2.9"),
                true,
                false,
                true
            ),
            Plan::Continue
        );
    }

    #[test]
    fn opt_out_notices_a_mismatch_without_delegating() {
        let plan = decide(
            Some(("nub".into(), Some("0.1.0".into()))),
            &v("0.2.9"),
            false,
            true,
            true,
        );
        match plan {
            Plan::Notice(msg) => {
                assert!(msg.contains("0.1.0") && msg.contains("0.2.9"));
                assert!(msg.contains("NUB_SELF_SHIM=0"));
            }
            other => panic!("expected a Notice, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_platform_falls_through_in_process() {
        // A default-on provision that can't succeed on a whole platform must NOT
        // brick every install there — it runs in-process with a notice.
        match decide(
            Some(("nub".into(), Some("0.1.0".into()))),
            &v("0.2.9"),
            false,
            false,
            false,
        ) {
            Plan::Notice(msg) => assert!(
                msg.contains("no build for this platform") && msg.contains("NUB_SELF_SHIM=0"),
                "notice: {msg}"
            ),
            other => panic!("expected a Notice, got {other:?}"),
        }
    }

    #[test]
    fn non_exact_pin_notices_and_does_not_delegate() {
        for spec in ["0.2", "latest", "^0.2.0"] {
            match decide(
                Some(("nub".into(), Some(spec.into()))),
                &v("0.2.9"),
                false,
                false,
                true,
            ) {
                Plan::Notice(msg) => assert!(
                    msg.contains(spec) && msg.contains("exact"),
                    "notice for {spec}: {msg}"
                ),
                other => panic!("expected a Notice for {spec}, got {other:?}"),
            }
        }
    }

    /// The `+sha512` regression, end-to-end through the real field reader: nub's
    /// own #255 stamp is `nub@<v>` (bare today) but a hand-written or future
    /// `+sha512`-suffixed self-pin must strip to the bare version so a matching pin
    /// is a silent no-op — never a spurious delegation on nub's own stamp.
    #[test]
    fn sha512_suffixed_self_pin_reads_stripped_and_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","version":"1.0.0","packageManager":"nub@0.2.9+sha512.deadbeefcafe"}"#,
        )
        .unwrap();
        let declared = nub_core::pm::resolve::declared_package_manager_field(dir.path());
        assert_eq!(
            declared,
            Some(("nub".to_string(), Some("0.2.9".to_string()))),
            "the field reader must strip the +sha512 build metadata"
        );
        assert_eq!(
            decide(declared, &v("0.2.9"), false, false, true),
            Plan::Continue,
            "a suffixed self-pin equal to the running version must NOT delegate"
        );
    }

    /// The self-shim reads the exact `packageManager` field ONLY — a nub named in
    /// the `devEngines` range-of-intent must NOT drive a provision or a not-exact
    /// notice (that would nag on every command for a legitimate range).
    #[test]
    fn devengines_nub_is_not_read_as_a_pin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","version":"1.0.0","devEngines":{"packageManager":{"name":"nub","version":">=0.1.0"}}}"#,
        )
        .unwrap();
        assert_eq!(
            nub_core::pm::resolve::declared_package_manager_field(dir.path()),
            None,
            "a devEngines-only nub entry is not the exact packageManager pin"
        );
    }
}
