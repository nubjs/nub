//! Conservative brand-literal lint — flags newly-introduced hardcoded
//! `aube <verb>` command references in user-facing runtime strings, steering
//! them to [`aube_util::cmd`] instead.
//!
//! Why this exists: an embedding host (the headline being nub, which embeds
//! aube as a library and rebrands its output) needs every *user-facing*
//! `aube install` / `aube run` / … reference to follow the active brand, not
//! be hardcoded to `aube`. The source-branding helpers `aube_util::prog()` /
//! `aube_util::cmd(verb)` are the seam: under the default profile they render
//! exactly `"aube"` / `"aube install"` byte-for-byte, and under an embedder
//! they carry the host's brand. This lint keeps future code on that seam — a
//! freshly-added `"aube install"` literal in a `miette!`/`bail!`/`eprintln!`
//! string trips it.
//!
//! Deliberately CONSERVATIVE (low false-positive). It only scans the command
//! layer (`src/commands/`), only flags string-literal lines (a `"` on the
//! line), only matches `aube <verb>` where `<verb>` is a real CLI command, and
//! skips everything that is legitimately not a runtime command reference:
//! doc/line comments, *non-user-facing* `tracing::` logs (`debug!`/`trace!`/
//! `info!` — see the severity split below), branded filenames
//! (`aube-lock.yaml`, `aube-workspace.yaml` use `aube-`, never `aube `),
//! manifest keys (`package.json#aube`, `aube.<key>`), sentence-start
//! capitalized prose ("Aube …"), and — importantly — **clap help text**, which
//! is the one user-facing surface that genuinely *cannot* use the runtime
//! helper. `#[command(after_long_help = ...)]` and `long_about` take a
//! `&'static str` resolved at clap-definition time, so a help const can't call
//! `cmd()`; the lint exempts the `pub const ..._HELP` blocks and the
//! `$ aube <verb>` shell-example lines they contain. (Branding help text is a
//! separate, clap-level concern tracked as a follow-up.) An explicit
//! [`ALLOWLIST`] covers any remaining one-off intentional mention.
//!
//! ## `tracing::` severity split — why `warn!`/`error!` ARE scanned
//! Not all `tracing::` lines are internal. `tracing::debug!` / `trace!` /
//! `info!` are developer logs the user never sees on a normal run, so they may
//! contain `aube <verb>` freely and stay exempt. But `tracing::warn!` and
//! `tracing::error!` SURFACE to the user (the default subscriber prints them),
//! so a command hint inside one — ``"Run `aube approve-builds`"``,
//! ``"run `aube install --no-frozen-lockfile`"`` — is exactly as user-facing as
//! the same hint in a `bail!`/`eprintln!`, and is held to the same `cmd()`
//! contract. The lint therefore scans `warn!`/`error!` with the identical
//! `aube <verb>` verb matcher (NOT a blanket "contains aube" — a `warn!` that
//! mentions `aube` for a non-command reason, e.g. a path or proper noun, never
//! trips because the word after `aube ` isn't a CLI verb), and exempts only the
//! lower severities. The macro a line is judged under is the one whose
//! statement it sits inside (a multi-line `warn!(...)` is scanned across all of
//! its lines; the leading-word severity governs the whole call).
//!
//! ## How to satisfy this lint
//! Replace the hardcoded reference with the helper:
//!
//! ```ignore
//! // before — hardcoded brand, wrong under an embedder:
//! return Err(miette!("no lockfile found — run `aube install` first"));
//! // after — follows the active brand, byte-for-byte identical for standalone aube:
//! return Err(miette!("no lockfile found — run `{}` first", aube_util::cmd("install")));
//! ```
//!
//! For a bare program-name reference (not a `verb`) use `aube_util::prog()`.
//! If a flagged line is a genuine, intentional exception (a foreign-tool
//! example, a sentence whose rewrite would hurt clarity), add it to
//! [`ALLOWLIST`] below with a comment explaining why — keep that list short.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The set of real CLI command verbs, **derived from the clap command tree**
/// (`aube::command()`) rather than hand-maintained. A `aube <word>` literal
/// only trips the lint when `<word>` is one of these — so prose like "aube has
/// no shims" or "let aube do it" never trips (those words aren't commands),
/// keeping the lint conservative.
///
/// Deriving from clap is the whole point: the command surface is the single
/// source of truth, so adding/renaming a verb (or an alias) updates the lint
/// automatically — no second list to keep in sync. We walk the command tree
/// recursively, so nested subcommands (`store prune`, `config get`, …) and the
/// leading word of multi-word chains are all covered, and we include every
/// subcommand's visible *and* hidden aliases (`i`/`install`, `rm`/`remove`,
/// `it`/`install-test`, `t`/`test`, …) since a hardcoded `aube <alias>` is just
/// as much a brand leak as the canonical spelling.
static VERBS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    let mut verbs = BTreeSet::new();
    collect_verbs(&aube::command(), &mut verbs);
    verbs
});

/// Walk the clap command tree, collecting every subcommand name plus all of its
/// aliases (visible and hidden), recursing into nested subcommands so the whole
/// surface — top-level verbs, their aliases, and nested chains — is covered.
/// The `external_subcommand` catch-all has no fixed name and contributes
/// nothing; names carrying chars the `aube <verb>` matcher can't reach (e.g. the
/// hidden `__node-gyp-bootstrap`) are harmless — they simply never match a
/// literal.
fn collect_verbs(cmd: &clap::Command, out: &mut BTreeSet<String>) {
    for sub in cmd.get_subcommands() {
        out.insert(sub.get_name().to_string());
        for alias in sub.get_all_aliases() {
            out.insert(alias.to_string());
        }
        collect_verbs(sub, out);
    }
}

/// Intentional exceptions: `crate/path` substrings or exact source lines where
/// an `aube <verb>`-shaped literal is deliberately left as-is. Keep this SHORT
/// — each entry is a place the embedder brand will NOT follow, so it must be a
/// genuine non-command-reference (prose, a foreign example) and not a missed
/// conversion. Match is by `line.contains(entry)`.
const ALLOWLIST: &[&str] = &[
    // A provenance/origin *data value* (the `RuntimeRequest::origin` PathBuf
    // recording where a Node pin came from), not user-facing emission text.
    // It's diagnostic metadata, the byte string is matched elsewhere, and it's
    // built where a `&str`/`PathBuf` is expected — not a place a runtime helper
    // belongs.
    "PathBuf::from(\"aube runtime set",
];

fn commands_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir commands") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Opens a clap help-text const block: `pub const <NAME>_HELP: &str = "..."`
/// (the `AFTER_LONG_HELP` / `CHECK_AFTER_LONG_HELP` convention). The string
/// runs across many lines until a `";` terminator.
fn is_help_const_open(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("pub const ") && t.contains("_HELP") && t.contains(": &str")
}

/// `tracing::` severities that are *not* user-facing: developer logs that the
/// default subscriber doesn't surface on a normal run. Lines inside one of
/// these (and their multi-line continuations) are exempt — they may contain
/// `aube <verb>` freely. Conversely `warn!` / `error!` DO surface to the user
/// and are scanned like any other emission macro (see module docs, severity
/// split). Returns the level word if `line` opens a `tracing::<level>!` call.
fn tracing_level(line: &str) -> Option<&str> {
    let rest = line.split_once("tracing::")?.1;
    let level: &str = rest
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()?;
    (!level.is_empty()).then_some(level)
}

/// Is this `tracing::<level>` an internal (non-user-facing) log? `debug`,
/// `trace`, `info` — everything that is NOT `warn`/`error`. A `tracing::warn!`
/// or `tracing::error!` is user-facing and must be scanned.
fn is_internal_tracing_level(level: &str) -> bool {
    !matches!(level, "warn" | "error")
}

/// A line is exempt when it can't be a *runtime* user-facing command
/// reference. `in_exempt_region` is the region flag for the clap help-text
/// blocks and internal (`debug`/`trace`/`info`) multi-line tracing calls (see
/// the scan loop in [`no_hardcoded_aube_verb_in_user_facing_strings`]).
fn is_exempt(line: &str, in_exempt_region: bool) -> bool {
    let t = line.trim_start();
    // Comments (line + doc).
    if t.starts_with("//") {
        return true;
    }
    // Internal logging is not user-facing — but only the lower severities.
    // `tracing::warn!` / `tracing::error!` surface to the user and ARE scanned.
    if tracing_level(line).is_some_and(is_internal_tracing_level) {
        return true;
    }
    // Clap help text — a `&'static str` resolved at definition time, can't call
    // the runtime helper. The whole `pub const ..._HELP` block is exempt, plus
    // any `$ aube <verb>` shell-example line (the canonical help-example shape).
    // Also covers internal multi-line tracing (`debug`/`trace`/`info`) regions.
    if in_exempt_region || t.contains("$ aube ") {
        return true;
    }
    // Must be a string literal to be emitted text at all.
    if !line.contains('"') {
        return true;
    }
    for skip in ALLOWLIST {
        if line.contains(skip) {
            return true;
        }
    }
    false
}

/// Does this line contain a hardcoded `aube <verb>` command reference that
/// should be `aube_util::cmd(...)`? Returns the offending verb if so.
fn offending_verb(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find("aube ") {
        let at = search_from + rel;
        search_from = at + 5;
        // Reject branded *filenames* / manifest keys: `aube-lock.yaml` and
        // friends use `aube-`, never `aube ` (space), so the "aube " match
        // already excludes them. But also reject when the char *before* the
        // match is alphanumeric (`...aube ` inside another word/path) — keep it
        // a word boundary.
        if at > 0 {
            let prev = bytes[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'.' {
                continue;
            }
        }
        // The word after "aube ".
        let rest = &line[at + 5..];
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        if VERBS.contains(&word) {
            return Some(word);
        }
    }
    None
}

/// The command layer must not hardcode `aube <verb>` in user-facing strings —
/// use `aube_util::cmd(verb)` so the reference follows the active embedder
/// brand. See this file's module docs for how to satisfy the lint.
#[test]
fn no_hardcoded_aube_verb_in_user_facing_strings() {
    let mut files = Vec::new();
    rs_files(&commands_dir(), &mut files);
    assert!(
        !files.is_empty(),
        "found no command-layer source files to lint"
    );

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut hits = Vec::new();
    for file in &files {
        let src = fs::read_to_string(file).expect("read source");
        // Region flags. A `pub const ..._HELP: &str = "..."` clap help block and
        // a multi-line *internal* `tracing::{debug,trace,info}!(...)` call both
        // span several lines; their inner string lines are exempt (help can't
        // use the runtime helper; low-severity tracing is internal). A
        // multi-line `tracing::warn!`/`error!` is NOT exempt — it surfaces to
        // the user — so its continuation lines are scanned normally. Track
        // whether we're inside an exempt region.
        let mut in_help_const = false;
        let mut in_internal_tracing = false;
        for (i, line) in src.lines().enumerate() {
            if !in_help_const && is_help_const_open(line) {
                in_help_const = true;
            }
            // Open an internal-tracing region only for the non-user-facing
            // severities; `warn!`/`error!` deliberately do not start one.
            if !in_internal_tracing
                && !line.trim_end().contains(';')
                && tracing_level(line).is_some_and(is_internal_tracing_level)
            {
                in_internal_tracing = true;
            }
            let exempt = is_exempt(line, in_help_const || in_internal_tracing);
            // A help block ends on its closing `";`; a tracing call ends on the
            // line that closes its statement with `;`.
            if in_help_const && line.contains("\";") {
                in_help_const = false;
            }
            if in_internal_tracing && line.trim_end().ends_with(';') {
                in_internal_tracing = false;
            }
            if exempt {
                continue;
            }
            if let Some(verb) = offending_verb(line) {
                let rel = file.strip_prefix(manifest).unwrap_or(file);
                hits.push(format!(
                    "{}:{}: hardcoded `aube {verb}` — use `aube_util::cmd(\"{verb}\")`\n    {}",
                    rel.display(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "hardcoded `aube <verb>` command reference(s) in user-facing strings — \
         replace with `aube_util::cmd(<verb>)` so the brand follows the active \
         embedder (it renders `aube <verb>` byte-for-byte under standalone aube). \
         If a hit is a genuine non-command-reference, allowlist it in \
         brand_literal_lint.rs with a justifying comment.\n\n{}",
        hits.join("\n")
    );
}

/// The clap-derived verb set is non-empty and covers the canonical core verbs.
/// Guards against a future refactor that silently breaks the derivation (e.g.
/// `aube::command()` no longer exposing subcommands) and leaves the lint matching
/// nothing — which would pass vacuously while catching zero real leaks.
#[test]
fn derived_verbs_cover_the_core_surface() {
    assert!(
        VERBS.len() > 20,
        "expected the clap-derived verb set to be substantial, got {} — \
         did `aube::command()` stop exposing subcommands?",
        VERBS.len()
    );
    // Spot-check the verbs jdx specifically flagged (`test`, `install-test`) plus
    // a representative spread of canonical names, visible aliases, and the
    // multi-word/nested chains the old hand-list had to track by hand.
    for expect in [
        "install",
        "i",
        "add",
        "a",
        "remove",
        "rm",
        "test",
        "t",
        "install-test",
        "it",
        "run",
        "patch-commit",
        "approve-builds",
        "store",
        "prune",
        "config",
        "get",
    ] {
        assert!(
            VERBS.contains(expect),
            "clap-derived verb set is missing `{expect}` — the derivation lost a \
             verb the lint must catch"
        );
    }
}

/// A planted `aube <verb>` literal trips the matcher (positive control), and a
/// non-command `aube <word>` does not (conservativeness control).
#[test]
fn offending_verb_catches_planted_leak() {
    assert_eq!(
        offending_verb(r#"bail!("run `aube install` first")"#).as_deref(),
        Some("install")
    );
    // A word that isn't a CLI verb must not trip — keeps the lint low-false-positive.
    assert_eq!(offending_verb(r#"eprintln!("aube has no shims")"#), None);
}

/// The payoff of deriving from clap: a real CLI verb that was **never** in the
/// old hand-maintained list is now caught automatically. `version`, `cache`,
/// `diag`, and `sponsors` are all genuine subcommands the previous hardcoded
/// `VERBS` array omitted — under the static list a hardcoded `aube version`
/// brand leak would have slipped through; the dynamic derivation catches it,
/// proving the new approach is strictly stronger than the manual one.
#[test]
fn derivation_catches_verbs_the_old_hardcoded_list_missed() {
    for verb in ["version", "cache", "diag", "sponsors"] {
        assert!(
            VERBS.contains(verb),
            "`{verb}` is a real clap subcommand but the derived set lacks it"
        );
        let planted = format!(r#"bail!("see `aube {verb}` output")"#);
        assert_eq!(
            offending_verb(&planted).as_deref(),
            Some(verb),
            "dynamic lint should catch `aube {verb}` — a verb the old hardcoded \
             VERBS list did not contain"
        );
    }
}
