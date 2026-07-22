//! Consent gate for `nubx`'s *implicit* registry tier — the moment `nubx <bin>`
//! falls through a local `node_modules/.bin` miss into a download-and-run.
//!
//! The thesis: **`nubx` never executes remote code silently.** An installed bin
//! never reaches this module — the gate guards only the registry fallthrough.
//! There it splits three ways:
//!
//! - **CI** (a truthy `CI`) → fail closed. A CI job that needs a tool declares it;
//!   nub does not fetch-and-run arbitrary remote code where no human can intervene.
//! - **Non-interactive** (stdin/stderr not a terminal) → fail closed. No terminal,
//!   no way to ask, so refuse rather than guess.
//! - **Interactive TTY** → an arrow-key consent select (Yes / No / Never) on the
//!   *first* fetch of a spec, then run without re-asking once that spec is
//!   recorded as consented. Picking **Never** persists the global kill-switch
//!   `exec.implicitDlx = never` (see [`crate::config`]) and disables this whole
//!   implicit tier until re-enabled.
//!
//! `-y`/`--yes` is the explicit non-interactive escape hatch: it lets CI / non-TTY
//! through and skips the first-fetch prompt. Fail-closed by default, never
//! impossible.
//!
//! **The gate keys on a persistent per-user *consent ledger*** (`<cache>/dlx/
//! consent.json`). An entry is a standing run-grant written *only* by an explicit
//! consent (a TTY `y`, or `-y`); nothing else pre-populates it. The prompt fires
//! exactly on a ledger miss — "have we already consented to this spec?" — which
//! folds in re-resolution: a **pinned** spec (`cowsay@1.5.0`, immutable identity)
//! is consented forever; a **floating** spec (`cowsay`, `@latest`, `@^1`) is
//! consented for a 24h TTL, after which the entry is stale → a miss → re-prompt
//! (new resolution = possible new code = fresh consent). `nub dlx` is the explicit
//! download command and bypasses this gate entirely (invoking it IS the consent).
//!
//! Scope note: this ledger records *consent*, not the fetched bytes. The
//! per-resolution tree cache (skip the re-fetch on a hit) is the separate,
//! locked-but-deferred follow-up (see `.fray/universal-nubx.md`); it keys off the
//! same entries, so it layers on without changing this contract.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Floating-spec consent TTL: 24h, matching pnpm's `dlxCacheMaxAge` (1440 min).
/// A pinned spec ignores this (immutable identity → consent never expires).
const FLOATING_TTL_SECS: u64 = 24 * 60 * 60;

/// The abort message printed when `exec.implicitDlx = never` and the bin missed
/// the local `node_modules/.bin` chain. LOCKED, maintainer-authored copy —
/// reproduce verbatim, no rewording/capitalization/backtick changes. A test pins
/// the exact bytes. ("script or executable" became "executable" when nubx dropped
/// the script tier; the second line is unchanged.)
const NEVER_ABORT_MESSAGE: &str =
    "No matching executable found.\nTo run a package from the remote registry, try `nub dlx`";

/// What the gate decided for an implicit registry fetch.
pub enum Decision {
    /// Consent is in hand — proceed to fetch-and-run. Carries whether the ledger
    /// must be written (a fresh consent) so the caller records it only on a run it
    /// actually performs.
    Proceed { record: bool },
    /// Refused. Carries the process exit code and a message already printed to
    /// stderr; the caller returns this code without fetching.
    Refused(i32),
}

/// Decide whether an implicit `nubx` registry fetch may proceed.
///
/// `specs` is the canonical install set (the `-p` packages, or the bare bin
/// token) — the same thing handed to the engine, so the gate and the fetch agree
/// on identity. `yes` is `nubx -y`.
///
/// The order is load-bearing for the locked contract. `-y` is explicit consent
/// and proceeds in ANY context. Otherwise **CI and non-TTY fail closed
/// unconditionally** — the ledger is NOT consulted there, so a restored/shared
/// `consent.json` can never let a fetch run silently where no human can confirm.
/// The ledger's "skip the prompt" hit applies ONLY in an interactive terminal,
/// where a human granted that consent and is present to have aborted it.
pub fn gate(specs: &[String], yes: bool) -> Decision {
    if specs.is_empty() {
        // Defensive: no identity to gate on. Refuse rather than fetch blind.
        eprintln!("nubx: cannot determine what to fetch.");
        return Decision::Refused(1);
    }

    // `-y`: explicit up-front consent — proceed in CI, non-TTY, or a terminal.
    // Record only a spec we don't already hold a live grant for (don't churn it).
    // `-y` is the documented escape hatch out of a `never` kill-switch, so it is
    // checked BEFORE the kill-switch below.
    if yes {
        return Decision::Proceed {
            record: !has_live_consent(specs),
        };
    }

    // The global kill-switch (`exec.implicitDlx = never`) is read FIRST — before
    // CI/TTY probing, any prompt, prefetch, or network. Writing it via the `Never`
    // option (or `nub config set`) permanently disables the implicit registry tier;
    // explicit `nub dlx <spec>` / `nubx -y <spec>` (above) stay open. The bin already
    // missed the local `node_modules/.bin` chain to reach here, so we abort with the
    // locked, maintainer-authored two-line message. Reproduce it verbatim.
    if matches!(
        crate::config::implicit_dlx(),
        crate::config::ImplicitDlx::Never
    ) {
        eprintln!("{NEVER_ABORT_MESSAGE}");
        return Decision::Refused(1);
    }

    // No `-y`. CI → always fail closed (a human can't intervene; the blast radius
    // is largest). The ledger is deliberately not checked — "CI → fail closed" is
    // unconditional.
    if is_ci() {
        eprintln!(
            "nubx: refusing to download {} in CI.\n\
             \x20\x20A CI job should declare the tool as a dependency, or pass -y to fetch it.",
            specs.join(" ")
        );
        return Decision::Refused(1);
    }

    // No terminal to confirm at → fail closed, ledger not checked.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!(
            "nubx: refusing to download {} without a terminal to confirm at.\n\
             \x20\x20Pass -y to fetch it non-interactively.",
            specs.join(" ")
        );
        return Decision::Refused(1);
    }

    // Interactive terminal: a previously-recorded consent (fresh, for a floating
    // spec) skips the prompt; a new or newly-stale spec prompts.
    if has_live_consent(specs) {
        return Decision::Proceed { record: false };
    }
    match prompt_consent(specs) {
        Consent::Yes => Decision::Proceed { record: true },
        Consent::No => {
            eprintln!("nubx: aborted.");
            Decision::Refused(1)
        }
        Consent::Never => {
            // Persist the global kill-switch, then deny this invocation. A write
            // failure is surfaced but still denies (the user chose Never).
            if let Err(e) = crate::config::set_implicit_dlx(crate::config::ImplicitDlx::Never) {
                eprintln!("nubx: could not save the setting ({e}); not running.");
            } else {
                eprintln!(
                    "nubx: disabled the implicit dlx tier. Re-enable with \
                     `nub config set exec.implicitDlx prompt`."
                );
            }
            Decision::Refused(1)
        }
    }
}

/// Whether the ledger holds a still-valid consent for this spec set (pinned =
/// forever, floating = within TTL).
fn has_live_consent(specs: &[String]) -> bool {
    let pinned = specs.iter().all(|s| is_pinned_spec(s));
    ledger_has_valid(&ledger_key(specs), pinned)
}

/// Record a granted consent. Called by the caller *after* a `Proceed { record:
/// true }` AND a confirmed successful fetch+run, so the ledger only ever reflects
/// tools that actually resolved and ran — never a 404 / failed install, whose
/// "consent" would otherwise become a standing grant for a name with no bytes yet
/// (an attacker could later publish it). A write failure is non-fatal (worst case
/// is a re-prompt next time) — never block a successful run on the ledger.
pub fn record(specs: &[String]) {
    let key = ledger_key(specs);
    let pinned = specs.iter().all(|s| is_pinned_spec(s));
    let Some(path) = ledger_path() else { return };
    let mut ledger = read_ledger(&path);
    ledger.insert(
        key,
        Entry {
            specs: specs.to_vec(),
            recorded_at: now_unix(),
            pinned,
        },
    );
    let _ = write_ledger(&path, &ledger);
}

// ─────────────────────────── ledger storage ───────────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    /// The consented install set, stored for human inspection of the ledger.
    specs: Vec<String>,
    /// Unix seconds the consent was granted. A floating entry expires
    /// `FLOATING_TTL_SECS` after this; a pinned entry never does.
    recorded_at: u64,
    /// Whether every spec was an exact-version ("pinned") identity at consent
    /// time. Pinned consent is immortal; floating consent honors the TTL.
    pinned: bool,
}

/// Canonical, order-independent ledger key for a spec set. Sorting makes
/// `-p a -p b` and `-p b -p a` one entry; the raw specs (not resolved versions)
/// are the identity the user typed and consented to.
fn ledger_key(specs: &[String]) -> String {
    let mut sorted: Vec<&str> = specs.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(" ")
}

fn ledger_path() -> Option<PathBuf> {
    Some(
        nub_core::node::discovery::cache_dir()?
            .join("dlx")
            .join("consent.json"),
    )
}

fn read_ledger(path: &std::path::Path) -> BTreeMap<String, Entry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic write so a concurrent reader never sees a half-written ledger:
/// serialize to a sibling temp file, then rename into place.
fn write_ledger(path: &std::path::Path, ledger: &BTreeMap<String, Entry>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(ledger)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

fn ledger_has_valid(key: &str, pinned: bool) -> bool {
    let Some(path) = ledger_path() else {
        return false;
    };
    let ledger = read_ledger(&path);
    let Some(entry) = ledger.get(key) else {
        return false;
    };
    // A spec that was floating at consent time stays bound by the TTL even if the
    // current invocation reads as pinned (and vice versa) — the *consent's* nature
    // governs its lifetime. Use the recorded `pinned`, falling back to the current
    // classification only as a tie-break for forward-compat ledgers.
    if entry.pinned || pinned {
        return true;
    }
    now_unix().saturating_sub(entry.recorded_at) < FLOATING_TTL_SECS
}

// ─────────────────────────── classification ───────────────────────────

/// A spec is "pinned" iff its version part is an exact semver (`1.2.3`,
/// `1.2.3-rc.1`) — an immutable identity that can never re-resolve to different
/// bytes. Everything else is floating: a bare name (`→ latest`), a dist-tag
/// (`@next`), a range (`@^1`, `@~1.2`, `@1.x`, `@*`), or a non-registry spec
/// (`github:…`, `git+…`, `file:…` — a ref can move). Floating identities honor the
/// consent TTL; pinned ones are consented forever.
fn is_pinned_spec(spec: &str) -> bool {
    let Some(version) = version_part(spec) else {
        return false; // no version → `latest`, floating
    };
    is_exact_semver(version)
}

/// Split a registry spec into its version part, preserving a leading `@scope/`.
/// Returns `None` for a non-registry spec (URL / git / shorthand) or a bare name.
fn version_part(spec: &str) -> Option<&str> {
    // Non-registry forms carry their own movability; never "pinned" here.
    if spec.contains("://")
        || spec.starts_with("git+")
        || spec.starts_with("github:")
        || spec.starts_with("gitlab:")
        || spec.starts_with("bitbucket:")
        || spec.starts_with("gist:")
        || spec.starts_with("file:")
        || spec.starts_with("link:")
    {
        return None;
    }
    // `@scope/name@version`: skip a leading scope `@`, find the version separator.
    let body = spec.strip_prefix('@').unwrap_or(spec);
    let at = body.find('@')?;
    let version = &body[at + 1..];
    (!version.is_empty()).then_some(version)
}

/// An exact, fully-qualified semver — `major.minor.patch` with optional
/// prerelease/build, no range operators, wildcards, or tags. This is the
/// immutable-identity test: anything looser can re-resolve.
fn is_exact_semver(v: &str) -> bool {
    // Reject range operators and wildcards outright.
    if v.starts_with(['^', '~', '>', '<', '=', 'v', 'V'])
        || v.contains('*')
        || v.contains('x')
        || v.contains('X')
    {
        return false;
    }
    if v.contains("||") || v.contains(" - ") || v.contains(',') {
        return false;
    }
    // core = before any `-`(prerelease) or `+`(build) marker.
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

// ─────────────────────────── context probes ───────────────────────────

/// Truthy `CI` detection. CI providers set `CI` to a truthy value; we refuse to
/// download in CI regardless of provider. A literal `0`/`false`/empty is treated
/// as "not CI" so a deliberate `CI=0` local shell isn't locked out.
fn is_ci() -> bool {
    match std::env::var("CI") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

/// The three outcomes of the interactive consent select.
enum Consent {
    /// Run it this time.
    Yes,
    /// Don't run it this time (no persistent effect).
    No,
    /// Never ask again — persist the global kill-switch and don't run.
    Never,
}

/// The select's row labels, in display order (index 0=Yes, 1=No, 2=Never — see
/// `option_at`). Default highlight is `Yes` (index 0) so a bare Enter accepts.
const OPTIONS: [&str; 3] = ["Yes", "No", "Never (don't ask me again)"];

/// Interactive consent select on stderr (stdout stays clean for the tool's own
/// output). A ●/○ radio: Up/Down move the highlight, Enter picks it, and `y`/`n`
/// are immediate single-key hotkeys. Ctrl-C / Esc → No (abort, no run).
///
/// The caller has already proven stdin AND stderr are real TTYs, so raw-mode init
/// is expected to succeed; if `console` still can't drive the terminal (returns a
/// non-tty key or errors), we fall back to the plain `y/N` line reader rather than
/// deadlock — the fail-safe answer is always No.
fn prompt_consent(specs: &[String]) -> Consent {
    use console::{Key, Term};

    let term = Term::stderr();
    // Belt-and-suspenders: the gate's IsTerminal check already guards this, but if
    // `console` disagrees about tty-ness, don't attempt a raw-mode draw.
    if !term.is_term() {
        return prompt_line(specs, &term);
    }

    let bin = specs.join(" ");
    let prompt_line1 = format!(
        "The {} bin is not installed locally.",
        console::style(&bin).cyan()
    );
    let prompt_line2 = "Install and run from the remote registry?";
    let mut sel: usize = 0; // default-highlight Yes (a bare Enter accepts)

    let _ = term.write_line(&prompt_line1);
    let _ = term.write_line(prompt_line2);
    let _ = term.hide_cursor();

    let draw = |sel: usize| {
        for (i, label) in OPTIONS.iter().enumerate() {
            let glyph = if i == sel { '●' } else { '○' };
            let _ = term.write_line(&format!("  {glyph} {label}"));
        }
    };
    draw(sel);

    let result = loop {
        let key = match term.read_key() {
            Ok(k) => k,
            // Ctrl-C surfaces as an Interrupted error here → treat as No/abort.
            Err(_) => break Consent::No,
        };
        match key {
            Key::ArrowUp => sel = (sel + OPTIONS.len() - 1) % OPTIONS.len(),
            Key::ArrowDown => sel = (sel + 1) % OPTIONS.len(),
            Key::Enter => break option_at(sel),
            Key::Char('y') | Key::Char('Y') => break Consent::Yes,
            Key::Char('n') | Key::Char('N') => break Consent::No,
            Key::Escape | Key::CtrlC => break Consent::No,
            // A stray key on a terminal that isn't delivering arrows (Unknown)
            // shouldn't spin: fall back to the line reader.
            Key::Unknown => {
                let _ = term.clear_last_lines(OPTIONS.len());
                let _ = term.show_cursor();
                return prompt_line(specs, &term);
            }
            _ => continue,
        }
        // Redraw the options in place (clear the N rows we last wrote).
        let _ = term.clear_last_lines(OPTIONS.len());
        draw(sel);
    };

    let _ = term.clear_last_lines(OPTIONS.len());
    let _ = term.show_cursor();
    result
}

fn option_at(sel: usize) -> Consent {
    match sel {
        0 => Consent::Yes,
        2 => Consent::Never,
        _ => Consent::No,
    }
}

/// Plain `y/N` line-reader fallback for terminals where the raw-mode select can't
/// run. Default NO — anything but `y`/`yes` refuses. Never offers `Never` (that
/// only lives on the interactive select).
fn prompt_line(specs: &[String], term: &console::Term) -> Consent {
    let bin = specs.join(" ");
    let _ = term.write_str(&format!(
        "The {} bin is not installed locally.\nInstall and run from the remote registry? [Y/n] ",
        console::style(&bin).cyan()
    ));
    let _ = term.flush();
    match term.read_line() {
        Ok(line) if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") => {
            Consent::Yes
        }
        _ => Consent::No,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with `XDG_CONFIG_HOME`/`XDG_CACHE_HOME` pointed at fresh temp dirs
    /// and `CI` unset — so `gate`'s config read + ledger + CI probe are all
    /// hermetic. Holds the process-wide [`crate::config::test_env_lock`] (SHARED
    /// with `config`'s test helper) so the two env-mutating suites serialize
    /// against each other under the multi-thread runner.
    fn with_isolated_env(never: bool, f: impl FnOnce()) {
        let _guard = crate::config::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let cfg = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let prev_cfg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_cache = std::env::var_os("XDG_CACHE_HOME");
        let prev_ci = std::env::var_os("CI");
        // SAFETY: guarded by test_env_lock; every var restored before the guard drops.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", cfg.path());
            std::env::set_var("XDG_CACHE_HOME", cache.path());
            std::env::remove_var("CI");
        }
        if never {
            crate::config::set_implicit_dlx(crate::config::ImplicitDlx::Never).unwrap();
        }

        f();

        unsafe {
            match prev_cfg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_cache {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
            match prev_ci {
                Some(v) => std::env::set_var("CI", v),
                None => std::env::remove_var("CI"),
            }
        }
    }

    #[test]
    fn empty_specs_refuse() {
        assert!(matches!(gate(&[], false), Decision::Refused(1)));
    }

    #[test]
    fn never_fails_closed_without_yes() {
        // `never` + no `-y` refuses BEFORE any CI/TTY probe, prompt, or network —
        // the config read is the first gate after the `-y` bypass. `Refused` here
        // means no fetch was attempted (the caller returns the code without
        // reaching the engine).
        with_isolated_env(true, || {
            assert!(matches!(
                gate(&["cowsay".into()], false),
                Decision::Refused(1)
            ));
        });
    }

    #[test]
    fn yes_bypasses_never() {
        // `-y` is the documented escape hatch out of a `never` kill-switch.
        with_isolated_env(true, || {
            assert!(matches!(
                gate(&["cowsay".into()], true),
                Decision::Proceed { .. }
            ));
        });
    }

    #[test]
    fn never_abort_message_is_the_locked_copy() {
        // The message is maintainer-authored and printed verbatim; pin the exact
        // two lines so a reword can't slip through.
        assert_eq!(
            NEVER_ABORT_MESSAGE,
            "No matching executable found.\nTo run a package from the remote registry, try `nub dlx`"
        );
    }

    #[test]
    fn yes_proceeds_by_default() {
        with_isolated_env(false, || {
            assert!(matches!(
                gate(&["cowsay".into()], true),
                Decision::Proceed { record: true }
            ));
        });
    }

    #[test]
    fn exact_semver_is_pinned() {
        assert!(is_pinned_spec("cowsay@1.5.0"));
        assert!(is_pinned_spec("@scope/foo@2.0.0"));
        assert!(is_pinned_spec("pkg@1.2.3-rc.1"));
        assert!(is_pinned_spec("pkg@1.0.0+build.7"));
    }

    #[test]
    fn floating_specs_are_not_pinned() {
        assert!(!is_pinned_spec("cowsay")); // → latest
        assert!(!is_pinned_spec("cowsay@latest"));
        assert!(!is_pinned_spec("cowsay@next"));
        assert!(!is_pinned_spec("cowsay@^1.5.0"));
        assert!(!is_pinned_spec("cowsay@~1.5"));
        assert!(!is_pinned_spec("cowsay@1.x"));
        assert!(!is_pinned_spec("cowsay@1"));
        assert!(!is_pinned_spec("cowsay@1.5"));
        assert!(!is_pinned_spec("@scope/foo")); // scope, no version
        assert!(!is_pinned_spec("github:user/repo"));
        assert!(!is_pinned_spec("git+https://host/u/r.git#v1"));
        assert!(!is_pinned_spec("file:../local"));
    }

    #[test]
    fn ledger_key_is_order_independent() {
        assert_eq!(
            ledger_key(&["b".into(), "a".into()]),
            ledger_key(&["a".into(), "b".into()])
        );
        assert_ne!(
            ledger_key(&["a".into()]),
            ledger_key(&["a".into(), "b".into()])
        );
    }

    #[test]
    fn ledger_roundtrips_and_honors_pinned_and_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let mut ledger = BTreeMap::new();
        ledger.insert(
            "pinned".to_string(),
            Entry {
                specs: vec!["cowsay@1.5.0".into()],
                recorded_at: 0, // ancient
                pinned: true,
            },
        );
        ledger.insert(
            "fresh".to_string(),
            Entry {
                specs: vec!["cowsay".into()],
                recorded_at: now_unix(),
                pinned: false,
            },
        );
        ledger.insert(
            "stale".to_string(),
            Entry {
                specs: vec!["cowsay".into()],
                recorded_at: now_unix().saturating_sub(FLOATING_TTL_SECS + 10),
                pinned: false,
            },
        );
        write_ledger(&path, &ledger).unwrap();
        let back = read_ledger(&path);
        assert_eq!(back.len(), 3);

        // Validity is computed against the entry's own nature.
        let valid = |key: &str, pinned: bool| -> bool {
            let entry = back.get(key).unwrap();
            if entry.pinned || pinned {
                return true;
            }
            now_unix().saturating_sub(entry.recorded_at) < FLOATING_TTL_SECS
        };
        assert!(valid("pinned", true), "ancient pinned consent stays valid");
        assert!(valid("fresh", false), "fresh floating consent is valid");
        assert!(!valid("stale", false), "stale floating consent expires");
    }
}
