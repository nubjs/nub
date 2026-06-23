//! Presentation layer for engine output. **All** engine text that reaches
//! nub's stdout/stderr flows through here — error reports, warnings, and the
//! occasional informational line a family verb relays.
//!
//! Three jobs (the maintainer's hard requirement: no `ERR_AUBE_*`/`WARN_AUBE_*`
//! strings and no `aube.jdx.dev` URLs may reach nub's output; the rewrite
//! happens at presentation time, never by renaming codes in the fork):
//!
//! 1. **Rewrite** rendered text: `ERR_AUBE_*` → `ERR_NUB_*`, `WARN_AUBE_*` →
//!    `WARN_NUB_*`, `aube.jdx.dev` URLs stripped (a line left holding only a
//!    dangling label like `Details:` is dropped whole), and message-level
//!    `aube`/`Aube` verb/binary spellings rebranded to `nub` — while
//!    preserving real on-disk names (`aube-lock.yaml`,
//!    `aube-workspace.yaml`, `.aube-state`, path segments like
//!    `share/aube/store`) that genuinely exist in the user's project or
//!    filesystem.
//! 2. **Exit codes**: map a failing report's diagnostic code through the
//!    engine's own exit table (`aube_codes::exit::EXIT_TABLE`), mirroring
//!    aube's `cli_main` (`vendor/aube/crates/aube/src/lib.rs::
//!    report_exit_code`): codeless or unlisted codes exit
//!    [`aube_codes::exit::EXIT_GENERIC`] (1).
//! 3. **Passthrough**: [`warn`] / [`info`] for family code that emits its
//!    own lines — both route through the same rewrite so no call site can
//!    bypass the brand boundary.
//!
//! Known accepted gaps (documented, not bugs): a sentence-final bare
//! `aube.` (token followed by `.`) is left alone — indistinguishable from a
//! domain/filename prefix without grammar; the multicall spellings
//! `aubx`/`aubr` are not rewritten (they never appear in the embedded
//! command layer's messages).

/// Render a failing engine report for nub's stderr: miette's fancy Debug
/// render (exactly what aube's own `cli_main` prints), then the brand
/// rewrite.
pub(crate) fn render_report(report: &miette::Report) -> String {
    rewrite(&format!("{report:?}"))
}

/// Print a failing engine report to stderr and return the exit code its
/// diagnostic maps to. The one-stop failure path for family verbs.
pub(crate) fn emit_report(report: &miette::Report) -> i32 {
    eprintln!("{}", render_report(report));
    exit_code(report)
}

/// Resolve a report's exit code against the engine's own table, mirroring
/// `vendor/aube/crates/aube/src/lib.rs::report_exit_code`: the diagnostic's
/// `code()` is looked up in `aube_codes::exit::EXIT_TABLE`; no code or no
/// entry falls back to `EXIT_GENERIC` (1).
pub(crate) fn exit_code(report: &miette::Report) -> i32 {
    report
        .code()
        .and_then(|code| aube_codes::exit::exit_code_for(&code.to_string()))
        .unwrap_or(aube_codes::exit::EXIT_GENERIC)
}

/// Warning passthrough for family verbs (stderr, rewritten). Use for
/// non-fatal engine-adjacent notices.
#[allow(dead_code)] // first consumers land with the family fill-ins
pub(crate) fn warn(msg: &str) {
    eprintln!("{}", rewrite(msg));
}

/// Info passthrough for family verbs (stderr, rewritten — the engine's
/// convention: stdout is data, progress/status lines go to stderr).
pub(crate) fn info(msg: &str) {
    eprintln!("{}", rewrite(msg));
}

/// Help-grade rewrite: the config-vocabulary map below, then [`rewrite`].
///
/// Help text describes nub's *configured contract* — not runtime facts — so
/// engine spellings of configuration locations are mapped to what nub's
/// embedder seams actually configure: `defaultLockfileFormat=pnpm` (fresh
/// lockfiles are `pnpm-lock.yaml`), the workspace-yaml list restricted to
/// `pnpm-workspace.yaml`, the manifest config namespace restricted to
/// `pnpm`, and `virtualStoreDir=node_modules/.nub`. Runtime messages must
/// keep using [`rewrite`]: they may truthfully name on-disk files (a real
/// `aube-lock.yaml` sitting in a project), which the word pass deliberately
/// preserves and this map would falsify.
///
/// Also used for clap usage errors (same rendering path as help). Corner:
/// a user-typed argument that happens to contain one of these spellings
/// would be echoed back mapped — accepted, the echo still names a file the
/// engine would treat identically.
pub(crate) fn rewrite_help(text: impl AsRef<str>) -> String {
    const VOCAB: &[(&str, &str)] = &[
        // Upstream lists both names; nub's list is pnpm-only — dedupe
        // before the generic mapping below would double the survivor.
        (
            "`aube-workspace.yaml`, `pnpm-workspace.yaml`",
            "`pnpm-workspace.yaml`",
        ),
        ("aube-workspace.yaml", "pnpm-workspace.yaml"),
        ("aube-lock.yaml", "pnpm-lock.yaml"),
        // `set --location` long help, dotted-map paragraph: upstream edits
        // map entries in the workspace yaml or a `package.json#aube.<map>`
        // field; nub refuses map writes outright (brand boundary — the
        // manifest fallback would plant a foreign-brand field). The help
        // must state the refusal, not upstream's write behavior. Listed
        // before the generic `package.json#aube.` mapping below, which
        // would otherwise rewrite this paragraph first and break the match.
        (
            "Dotted writes for aube map settings (`allowBuilds.<pkg>`, \
             `overrides.<pkg>`, …) edit one entry at a time. At project scope \
             (`--local`) they land in `pnpm-workspace.yaml#<map>.<entry>` or \
             `package.json#aube.<map>.<entry>` if no workspace yaml exists, the \
             same place install reads from. User-scope dotted writes for these \
             maps error: aube only reads them per project.",
            "Workspace map settings (`allowBuilds.<pkg>`, `overrides.<pkg>`, …) \
             are refused at any location: add the entry under the map in \
             `pnpm-workspace.yaml` instead (for dependency build scripts, \
             `approve-builds` manages the `allowBuilds` list).",
        ),
        // "the aube/pnpm global directory", "aube/pnpm sidecar entries" —
        // prose, never a path token.
        ("aube/pnpm", "pnpm"),
        ("package.json#aube.", "package.json#pnpm."),
        // `$AUBE_HOME` is invisible to nub (env families); the registry
        // location is described structurally instead.
        (
            "`$AUBE_HOME/global-links`",
            "`global-links` in the engine's data directory",
        ),
        // `why --paths` example path: nub's virtualStoreDir default.
        (".aube/<dep_path>", ".nub/<dep_path>"),
        // The GVS location in engine docs;
        // described structurally — the literal path is engine cache state.
        (
            "`~/.cache/aube/virtual-store/`",
            "the global virtual-store cache",
        ),
        // The engine's own config file (config --location docs); described
        // structurally like the GVS path — the literal `.config/aube`
        // location is engine state (re-homing it is an open fork item,
        // same as cacheDir).
        (
            "(`~/.config/aube/config.toml` + `~/.npmrc`)",
            "(the engine's user config + `~/.npmrc`)",
        ),
        (
            "(`~/.config/aube/config.toml` for known aube settings",
            "(the engine's user config for known engine settings",
        ),
        // `set --location` long help: upstream routes non-npm-shared keys
        // to its own config.toml; nub mirrors pnpm v11 instead (the
        // store_config_family module doc) — non-shared scalars go to
        // `pnpm-workspace.yaml` under a pnpm incumbent, else the project
        // `.npmrc`; `--location`/`--local` are ignored for these. The help
        // must describe nub's contract, divergence included.
        (
            "land in aube's own config (`~/.config/aube/config.toml` at user scope, \
             `<cwd>/.config/aube/config.toml` at project scope) where sibling tools \
             don't see them",
            "are written to `pnpm-workspace.yaml` in a pnpm project, else the project \
             `.npmrc` — the same files install reads — regardless of \
             `--location`/`--local` (the confirmation line names the file written)",
        ),
        // `patch-commit`'s arg help names the engine's on-disk state
        // sidecar; help describes the mechanism structurally.
        (
            "the `.aube_patch_state.json` sidecar",
            "the patch-state sidecar",
        ),
        // "Aube-only and pnpm-only settings" — prose, but the `-` welds
        // the brand to the suffix so the word pass must preserve it.
        ("Aube-only", "Engine-only"),
    ];
    let mut text = text.as_ref().to_string();
    for (from, to) in VOCAB {
        text = text.replace(from, to);
    }
    rewrite(&text)
}

/// Runtime hint-context corrections nub applies on top of the brand rewrite.
/// These are NOT brand leaks (the branded workspace/lockfile-name leaks are
/// fixed at the engine source via the embedder seam — `aube_util::
/// workspace_markers()` / `lockfile_basename()` — so they never reach this
/// layer, and would be split mid-token by miette's line-wrapping anyway).
/// They are messages that are brand-clean but factually misleading for nub:
///
/// - The frozen-install / `nub ci` missing-lockfile hint names a single
///   lockfile to commit (`pnpm-lock.yaml`), but which file a fresh
///   `nub install` writes is context-dependent (`lock.yaml` for a truly-fresh
///   project, `pnpm-lock.yaml` otherwise), so naming one is misleading.
///   Neutralized to "a lockfile" — a nub-only correction (standalone aube's
///   `pnpm-lock.yaml` hint is correct for it, so this stays out of the fork).
const RUNTIME_HINT_VOCAB: &[(&str, &str)] = &[(
    "commit pnpm-lock.yaml to your repository",
    "commit a lockfile to your repository",
)];

/// The brand rewrite. Order matters: the runtime-hint corrections first (a
/// plain phrase substitution before any brand pass touches the text), then
/// codes (so the `AUBE` inside `ERR_AUBE_*` never reaches the word pass), then
/// per-line URL stripping, then the word-boundary `aube` → `nub` pass.
pub(crate) fn rewrite(text: &str) -> String {
    let mut text = text.to_string();
    for (from, to) in RUNTIME_HINT_VOCAB {
        text = text.replace(from, to);
    }
    let text = text
        .replace("ERR_AUBE_", "ERR_NUB_")
        .replace("WARN_AUBE_", "WARN_NUB_")
        .replace(
            "`aube config set enableGlobalVirtualStore false`",
            "`enableGlobalVirtualStore=false` in `.npmrc` or set \
             `npm_config_enable_global_virtual_store=false`",
        );
    let mut out = String::with_capacity(text.len());
    let mut emitted = false;
    for line in text.split('\n') {
        let Some(line) = strip_engine_urls(line) else {
            continue; // line reduced to a dangling label — drop it whole
        };
        if emitted {
            out.push('\n');
        }
        out.push_str(&rebrand_words(&line));
        emitted = true;
    }
    out
}

/// Host of the engine's documentation site. Any URL token containing it is
/// stripped; nub has no equivalent page to substitute, and pointing users at
/// another tool's docs is the leak this module exists to stop.
const ENGINE_DOC_HOST: &str = "aube.jdx.dev";

/// Remove engine-doc URL tokens from one line. A one-word introductory
/// label (`Details:`, `See:`) immediately before a stripped URL is removed
/// with it, whether the URL sits on its own line or inline mid-sentence.
/// Returns `None` when the whole line reduces to nothing but whitespace or a
/// dangling label that only existed to introduce the URL.
fn strip_engine_urls(line: &str) -> Option<String> {
    if !line.contains(ENGINE_DOC_HOST) {
        return Some(line.to_string());
    }
    let mut s = line.to_string();
    while let Some(at) = s.find(ENGINE_DOC_HOST) {
        // Expand to the whole whitespace-delimited token (catches the
        // https:// prefix and any path suffix).
        let mut start = s[..at]
            .rfind(char::is_whitespace)
            .map(|i| i + char::len_utf8(s[i..].chars().next().unwrap_or(' ')))
            .unwrap_or(0);
        let end = s[at..]
            .find(char::is_whitespace)
            .map(|i| at + i)
            .unwrap_or(s.len());
        // Also drop a one-word introductory label that only existed to
        // announce the URL (`Details: <url>`, `See: <url>`) and collapse the
        // gap it leaves. Without this, stripping an *inline* URL leaves a
        // dangling `Details: ` mid-line — the whole-line drop below only
        // fires when the label sits alone on its own line, which the
        // engine's single-line `tracing::warn!` messages (e.g. the
        // GVS-incompatible warning) never do.
        let before = s[..start].trim_end();
        let label_start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let label = &before[label_start..];
        if label.ends_with(':') && !label[..label.len() - 1].contains(char::is_whitespace) {
            // Take the label and the run of whitespace before it too, so the
            // surrounding sentence closes up cleanly.
            start = s[..label_start].trim_end().len();
            let suffix = s[end..].trim_start();
            s = if start == 0 {
                suffix.to_string()
            } else {
                format!("{} {suffix}", &s[..start])
            };
        } else {
            s.replace_range(start..end, "");
        }
    }
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.ends_with(':') {
        None
    } else {
        Some(s)
    }
}

/// Rebrand standalone `aube` / `Aube` word tokens to `nub` / `Nub`.
///
/// A token is "standalone" when the neighboring characters are not
/// name-continuation characters: alphanumerics, `.`, `-`, `_`, `/`, `@`,
/// `~`. That single rule keeps every real on-disk name intact —
/// `aube-lock.yaml` (next `-`), `.aube-state` (prev `.`), `aube.jdx.dev`
/// (next `.`), `aube_codes` (next `_`), `share/aube/store` (prev/next `/`)
/// — while catching the spellings that are genuinely the tool's name in
/// prose or a command line: `` `aube install` ``, `another aube install is
/// using this store`, `run "aube import"`.
fn rebrand_words(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = next_token(rest) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let token = &from[..4];
        let prev_ok = before.chars().next_back().is_none_or(is_word_boundary);
        let next_ok = from[4..].chars().next().is_none_or(is_word_boundary);
        if prev_ok && next_ok {
            out.push_str(if token == "Aube" { "Nub" } else { "nub" });
        } else {
            out.push_str(token);
        }
        rest = &from[4..];
    }
    out.push_str(rest);
    out
}

/// Byte offset of the next `aube`/`Aube` occurrence, if any.
fn next_token(s: &str) -> Option<usize> {
    match (s.find("aube"), s.find("Aube")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Characters that do NOT continue a file/path/identifier name. See
/// [`rebrand_words`].
fn is_word_boundary(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | '@' | '~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_diagnostic_codes_from_the_real_constants() {
        // Built from the real aube-codes constant so a fork-side rename
        // breaks this test instead of silently leaking the old code.
        let report = miette::miette!(
            code = aube_codes::errors::ERR_AUBE_NO_LOCKFILE,
            "no lockfile found and --frozen-lockfile is set"
        );
        let rendered = render_report(&report);
        assert!(
            rendered.contains("ERR_NUB_NO_LOCKFILE"),
            "rendered report must carry the rewritten code: {rendered}"
        );
        assert!(
            !rendered.contains("AUBE") && !rendered.contains("aube"),
            "no engine branding may survive the render: {rendered}"
        );
    }

    #[test]
    fn rewrites_message_level_engine_verb_spellings() {
        // The literal `nub ci` missing-lockfile hint from
        // vendor/aube/crates/aube/src/commands/install/mod.rs — the message
        // names the engine binary + verb, which must read as nub's.
        let msg = "no lockfile found and --frozen-lockfile is set\n\
                   help: commit pnpm-lock.yaml to your repository, or run \
                   `aube install --no-frozen-lockfile` to generate one";
        let out = rewrite(msg);
        assert!(
            out.contains("`nub install --no-frozen-lockfile`"),
            "verb spelling must be rebranded: {out}"
        );
        assert!(!out.contains("aube"), "{out}");
    }

    #[test]
    fn preserves_real_on_disk_engine_names() {
        // These name actual files/dirs that can exist in a user's project;
        // rewriting them would point users at paths that don't exist.
        for name in [
            "aube-lock.yaml",
            "aube-workspace.yaml",
            "node_modules/.nub/.aube-state",
            "node_modules/.aube-applied-patches.json",
            "~/.local/share/aube/store/v1",
        ] {
            assert_eq!(rewrite(name), name, "on-disk name must survive");
        }
        // …while the same stem as a standalone word still rebrands.
        assert_eq!(
            rewrite("another aube install is using this store"),
            "another nub install is using this store"
        );
    }

    #[test]
    fn strips_engine_doc_urls_and_dangling_labels() {
        // Shape of vendor/aube/crates/aube/src/commands/install/gvs.rs: the
        // URL sits on its own labeled line, which must vanish entirely.
        let msg = "global virtual store is not supported\n\
                   Details: https://aube.jdx.dev/package-manager/global-virtual-store\n\
                   use a per-project virtual store instead";
        let out = rewrite(msg);
        assert!(!out.contains("aube.jdx.dev"), "{out}");
        assert!(!out.contains("Details:"), "dangling label must drop: {out}");
        assert!(out.contains("use a per-project virtual store instead"));
        // Inline URL: only the token goes, the sentence stays.
        let inline = rewrite("see https://aube.jdx.dev/cli for flags");
        assert_eq!(inline, "see  for flags");
    }

    #[test]
    fn drops_inline_details_label_when_its_url_is_stripped() {
        // The live shape of the GVS-incompatible warning: the engine emits
        // it as one `tracing::warn!` line, so the `Details:` label and the
        // doc URL it introduces sit *inline*, and the warning layer appends
        // ` code=…` after them (log.rs). Stripping only the URL token left a
        // dangling `Details: ` mid-line — the exact leak seen on every
        // vite/next install. The introductory label must go with its URL.
        let line = "WARN `vite` isn't compatible with nub's global virtual \
                    store — installing per-project instead. To silence, run \
                    `aube config set enableGlobalVirtualStore false`. \
                    Details: https://aube.jdx.dev/package-manager/global-virtual-store \
                    code=WARN_NUB_GVS_INCOMPATIBLE";
        let out = rewrite(line);
        assert!(!out.contains("aube.jdx.dev"), "{out}");
        assert!(!out.contains("config set"), "{out}");
        assert!(out.contains("`enableGlobalVirtualStore=false` in `.npmrc`"));
        assert!(out.contains("`npm_config_enable_global_virtual_store=false`"));
        assert!(
            !out.contains("Details:"),
            "inline label introducing a stripped URL must drop: {out}"
        );
        // The substantive sentence and the trailing code field survive.
        assert!(out.contains("installing per-project instead"), "{out}");
        assert!(out.contains("code=WARN_NUB_GVS_INCOMPATIBLE"), "{out}");
        assert!(
            !out.contains("  "),
            "no double-space gap left behind: {out}"
        );
    }

    #[test]
    fn neutralizes_misleading_frozen_lockfile_name() {
        // The `nub ci` / `--frozen-lockfile` missing-lockfile hint names a
        // single lockfile to commit, but a fresh `nub install` may write
        // `lock.yaml` (truly-fresh) rather than `pnpm-lock.yaml` — so the
        // specific name is misleading. The corrected hint names no specific
        // file.
        let msg = "no lockfile found and --frozen-lockfile is set\n\
                   help: commit pnpm-lock.yaml to your repository, or run \
                   `aube install --no-frozen-lockfile` to generate one";
        let out = rewrite(msg);
        assert!(
            out.contains("commit a lockfile to your repository"),
            "the misleading specific lockfile name must be neutralized: {out}"
        );
        assert!(
            !out.contains("commit pnpm-lock.yaml"),
            "the specific (misleading) lockfile name must not survive: {out}"
        );
        // The actionable verb spelling still rebrands and survives.
        assert!(out.contains("`nub install --no-frozen-lockfile`"), "{out}");
    }

    #[test]
    fn exit_codes_follow_the_engine_exit_table() {
        // ERR_AUBE_NO_LOCKFILE carries bespoke exit 10 in the engine's
        // EXIT_TABLE (lockfile range); drift upstream should fail here.
        let coded = miette::miette!(code = aube_codes::errors::ERR_AUBE_NO_LOCKFILE, "x");
        assert_eq!(exit_code(&coded), 10);
        // Codeless reports (e.g. the ci missing-lockfile error) fall back
        // to the generic exit, matching aube's own cli_main.
        let plain = miette::miette!("no lockfile found");
        assert_eq!(exit_code(&plain), aube_codes::exit::EXIT_GENERIC);
    }
}
